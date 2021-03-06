use caption_strategy::{TypeCaptionStrategy, MethodCaptionStrategy};
use cpp_data::{CppData, CppVisibility, create_cast_method, CppTypeAllocationPlace};
use cpp_type::{CppTypeRole, CppType, CppTypeBase, CppTypeIndirection, CppTypeClassBase,
               CppFunctionPointerType};
use cpp_ffi_data::{CppAndFfiMethod, c_base_name, CppFfiHeaderData, QtSlotWrapper};
use cpp_method::{CppMethod, CppMethodKind, CppFunctionArgument, CppMethodClassMembership};
use common::errors::{Result, ChainErr, unexpected};
use common::log;
use common::utils::{MapIfOk, add_to_multihash};
use config::CppFfiGeneratorFilterFn;
use std::collections::{HashSet, HashMap};
use std::iter::once;

/// This object generates the C++ wrapper library
struct CppFfiGenerator<'a> {
  /// Input C++ data
  cpp_data: &'a CppData,
  /// Name of the wrapper library
  cpp_ffi_lib_name: String,
  /// FFI filters passed to `Config`
  filters: Vec<&'a Box<CppFfiGeneratorFilterFn>>,
}

/// Runs the FFI generator
pub fn run(cpp_data: &CppData,
           cpp_ffi_lib_name: String,
           filters: Vec<&Box<CppFfiGeneratorFilterFn>>)
           -> Result<Vec<CppFfiHeaderData>> {
  let generator = CppFfiGenerator {
    cpp_data: cpp_data,
    cpp_ffi_lib_name: cpp_ffi_lib_name,
    filters: filters,
  };

  let mut c_headers = Vec::new();
  let mut include_name_list: Vec<_> = generator
    .cpp_data
    .all_include_files()?
    .into_iter()
    .collect();
  include_name_list.sort();

  for include_file in &include_name_list {
    let mut include_file_base_name = include_file.clone();

    if let Some(index) = include_file_base_name.find('.') {
      include_file_base_name = include_file_base_name[0..index].to_string();
    }
    let methods = generator
      .process_methods(&include_file_base_name,
                       None,
                       generator
                         .cpp_data
                         .methods
                         .iter()
                         .filter(|x| &x.include_file == include_file))?;
    if methods.is_empty() {
      log::llog(log::DebugFfiSkips,
                || format!("Skipping empty include file {}", include_file));
    } else {
      c_headers.push(CppFfiHeaderData {
                       include_file_base_name: include_file_base_name,
                       methods: methods,
                       qt_slot_wrappers: Vec::new(),
                     });
    }
  }
  if let Some(header) = generator.generate_slot_wrappers()? {
    c_headers.push(header);
  }
  if c_headers.is_empty() {
    return Err("No FFI headers generated".into());
  }
  Ok(c_headers)
}

impl<'a> CppFfiGenerator<'a> {
  /// Returns false if the method is excluded from processing
  /// for some reason
  fn should_process_method(&self, method: &CppMethod) -> Result<bool> {
    if method.is_fake_inherited_method {
      return Ok(false);
    }
    let class_name = method.class_name().unwrap_or(&String::new()).clone();
    for filter in &self.filters {
      let allowed = filter(method)
        .chain_err(|| "cpp_ffi_generator_filter failed")?;
      if !allowed {
        log::llog(log::DebugFfiSkips,
                  || format!("Skipping blacklisted method: \n{}\n", method.short_text()));
        return Ok(false);
      }
    }
    if class_name == "QFlags" {
      return Ok(false);
    }
    if let Some(ref membership) = method.class_membership {
      if membership.kind == CppMethodKind::Constructor &&
         self.cpp_data.has_pure_virtual_methods(&class_name) {
        log::llog(log::DebugFfiSkips,
                  || format!("Skipping constructor of abstract class {}", class_name));
        return Ok(false);
      }
      if membership.visibility == CppVisibility::Private {
        return Ok(false);
      }
      if membership.visibility == CppVisibility::Protected {
        return Ok(false);
      }
      if membership.is_signal {
        return Ok(false);
      }
    }
    if method.template_arguments.is_some() {
      return Ok(false);
    }
    if method.template_arguments_values.is_some() && !method.is_ffi_whitelisted {
      // TODO: re-enable after template test compilation (#24) is implemented
      // TODO: QObject::findChild and QObject::findChildren should be allowed
      return Ok(false);
    }
    if method
         .all_involved_types()
         .iter()
         .any(|x| x.base.is_or_contains_template_parameter()) {
      return Ok(false);
    }
    Ok(true)
  }

  /// Generates FFI wrappers for all specified methods,
  /// resolving all name conflicts using additional method captions.
  fn process_methods<'b, I>(&self,
                            include_file_base_name: &str,
                            type_allocation_places_override: Option<CppTypeAllocationPlace>,
                            methods: I)
                            -> Result<Vec<CppAndFfiMethod>>
    where I: Iterator<Item = &'b CppMethod>
  {
    log::status(format!("Generating C++ FFI methods for header: {}",
                        include_file_base_name));
    let mut hash_name_to_methods: HashMap<String, Vec<_>> = HashMap::new();
    for method in methods {
      if !self.should_process_method(method)? {
        continue;
      }
      match method.to_ffi_signature(&self.cpp_data, type_allocation_places_override.clone()) {
        Err(msg) => {
          log::llog(log::DebugFfiSkips, || {
            format!("Unable to produce C function for method:\n{}\nError:{}\n",
                    method.short_text(),
                    msg)
          });
        }
        Ok(result) => {
          match c_base_name(&result.cpp_method,
                            &result.allocation_place,
                            include_file_base_name) {
            Err(msg) => {
              log::llog(log::DebugFfiSkips, || {
                format!("Unable to produce C function for method:\n{}\nError:{}\n",
                        method.short_text(),
                        msg)
              });
            }
            Ok(name) => {

              add_to_multihash(&mut hash_name_to_methods,
                               format!("{}_{}", &self.cpp_ffi_lib_name, name),
                               result);
            }
          }
        }
      }
    }

    let mut processed_methods = Vec::new();
    for (key, mut values) in hash_name_to_methods {
      if values.len() == 1 {
        processed_methods.push(CppAndFfiMethod::new(values.remove(0), key.clone()));
        continue;
      }
      let mut found_strategy = None;
      for strategy in MethodCaptionStrategy::all() {
        let mut type_captions = HashSet::new();
        let mut ok = true;
        for value in &values {
          let caption = value.c_signature.caption(strategy.clone())?;
          if type_captions.contains(&caption) {
            ok = false;
            break;
          }
          type_captions.insert(caption);
        }
        if ok {
          found_strategy = Some(strategy);
          break;
        }
      }
      if let Some(strategy) = found_strategy {
        for x in values {
          let caption = x.c_signature.caption(strategy.clone())?;
          let final_name = if caption.is_empty() {
            key.clone()
          } else {
            format!("{}_{}", key, caption)
          };
          processed_methods.push(CppAndFfiMethod::new(x, final_name));
        }
      } else {
        log::error(format!("values dump: {:?}\n", values));
        log::error("All type caption strategies have failed! Involved functions:");
        for value in values {
          log::error(format!("  {}", value.cpp_method.short_text()));
        }
        return Err(unexpected("all type caption strategies have failed").into());
      }
    }
    processed_methods.sort_by(|a, b| a.c_name.cmp(&b.c_name));
    Ok(processed_methods)
  }

  /// Generates slot wrappers for all encountered argument types
  /// (excluding types already handled in the dependencies).
  fn generate_slot_wrappers(&'a self) -> Result<Option<CppFfiHeaderData>> {
    let include_file_name = "slots";
    if self.cpp_data.signal_argument_types.is_empty() {
      return Ok(None);
    }
    let mut qt_slot_wrappers = Vec::new();
    let mut methods = Vec::new();
    for types in &self.cpp_data.signal_argument_types {
      let ffi_types = types
        .map_if_ok(|t| t.to_cpp_ffi_type(CppTypeRole::NotReturnType))?;
      let args_captions = types
        .map_if_ok(|t| t.caption(TypeCaptionStrategy::Full))?;
      let args_caption = if args_captions.is_empty() {
        "no_args".to_string()
      } else {
        args_captions.join("_")
      };
      let void_ptr = CppType {
        base: CppTypeBase::Void,
        indirection: CppTypeIndirection::Ptr,
        is_const: false,
        is_const2: false,
      };
      let func_arguments = once(void_ptr.clone())
        .chain(ffi_types.iter().map(|t| t.ffi_type.clone()))
        .collect();
      let class_name = format!("{}_SlotWrapper_{}", self.cpp_ffi_lib_name, args_caption);
      let function_type = CppFunctionPointerType {
        return_type: Box::new(CppType::void()),
        arguments: func_arguments,
        allows_variadic_arguments: false,
      };
      let create_function = |kind: CppMethodKind,
                             name: String,
                             is_slot: bool,
                             arguments: Vec<CppFunctionArgument>|
       -> CppMethod {
        CppMethod {
          name: name,
          class_membership: Some(CppMethodClassMembership {
                                   class_type: CppTypeClassBase {
                                     name: class_name.clone(),
                                     template_arguments: None,
                                   },
                                   is_virtual: true,
                                   is_pure_virtual: false,
                                   is_const: false,
                                   is_static: false,
                                   visibility: CppVisibility::Public,
                                   is_signal: false,
                                   is_slot: is_slot,
                                   kind: kind,
                                   fake: None,
                                 }),
          operator: None,
          return_type: CppType::void(),
          arguments: arguments,
          arguments_before_omitting: None,
          allows_variadic_arguments: false,
          include_file: include_file_name.to_string(),
          origin_location: None,
          template_arguments: None,
          template_arguments_values: None,
          declaration_code: None,
          doc: None,
          inheritance_chain: Vec::new(),
          is_ffi_whitelisted: false,
          is_unsafe_static_cast: false,
          is_direct_static_cast: false,
          is_fake_inherited_method: false,
        }
      };
      methods.push(create_function(CppMethodKind::Constructor,
                                   class_name.clone(),
                                   false,
                                   vec![]));
      methods.push(create_function(CppMethodKind::Destructor,
                                   format!("~{}", class_name),
                                   false,
                                   vec![]));
      let method_set_args = vec![CppFunctionArgument {
                                   name: "func".to_string(),
                                   argument_type: CppType {
                                     base: CppTypeBase::FunctionPointer(function_type.clone()),
                                     indirection: CppTypeIndirection::None,
                                     is_const: false,
                                     is_const2: false,
                                   },
                                   has_default_value: false,
                                 },
                                 CppFunctionArgument {
                                   name: "data".to_string(),
                                   argument_type: void_ptr.clone(),
                                   has_default_value: false,
                                 }];
      methods.push(create_function(CppMethodKind::Regular,
                                   "set".to_string(),
                                   false,
                                   method_set_args));

      let method_custom_slot = create_function(CppMethodKind::Regular,
                                               "custom_slot".to_string(),
                                               true,
                                               types
                                                 .iter()
                                                 .enumerate()
                                                 .map(|(num, t)| {
                                                        CppFunctionArgument {
                                                          name: format!("arg{}", num),
                                                          argument_type: t.clone(),
                                                          has_default_value: false,
                                                        }
                                                      })
                                                 .collect());
      let receiver_id = method_custom_slot.receiver_id()?;
      methods.push(method_custom_slot);
      qt_slot_wrappers.push(QtSlotWrapper {
                              class_name: class_name.clone(),
                              arguments: ffi_types,
                              function_type: function_type.clone(),
                              receiver_id: receiver_id,
                            });
      let cast_from = CppType {
        base: CppTypeBase::Class(CppTypeClassBase {
                                   name: class_name.clone(),
                                   template_arguments: None,
                                 }),
        indirection: CppTypeIndirection::Ptr,
        is_const: false,
        is_const2: false,
      };
      let cast_to = CppType {
        base: CppTypeBase::Class(CppTypeClassBase {
                                   name: "QObject".to_string(),
                                   template_arguments: None,
                                 }),
        indirection: CppTypeIndirection::Ptr,
        is_const: false,
        is_const2: false,
      };
      methods.push(create_cast_method("static_cast",
                                      &cast_from,
                                      &cast_to,
                                      false,
                                      true,
                                      include_file_name));
    }
    Ok(Some(CppFfiHeaderData {
              include_file_base_name: include_file_name.to_string(),
              methods: self
                .process_methods(include_file_name,
                                 Some(CppTypeAllocationPlace::Heap),
                                 methods.iter())?,
              qt_slot_wrappers: qt_slot_wrappers,
            }))
  }
}
