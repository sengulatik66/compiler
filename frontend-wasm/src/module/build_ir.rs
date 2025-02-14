use core::mem;

use miden_diagnostics::{DiagnosticsHandler, SourceSpan};
use miden_hir::{CallConv, ConstantData, Linkage, MidenAbiImport, ModuleBuilder, Symbol};
use wasmparser::{Validator, WasmFeatures};

use super::{module_translation_state::ModuleTranslationState, Module};
use crate::{
    error::WasmResult,
    intrinsics::is_miden_intrinsics_module,
    miden_abi::miden_sdk_function_type,
    module::{
        func_translator::FuncTranslator,
        module_env::{FunctionBodyData, ModuleEnvironment, ParsedModule},
        types::{ir_func_sig, ir_func_type, ir_type, ModuleTypes},
    },
    WasmError, WasmTranslationConfig,
};

/// Translate a valid Wasm core module binary into Miden IR module
pub fn translate_module(
    wasm: &[u8],
    config: &WasmTranslationConfig,
    diagnostics: &DiagnosticsHandler,
) -> WasmResult<miden_hir::Module> {
    let wasm_features = WasmFeatures::default();
    let mut validator = Validator::new_with_features(wasm_features);
    let parser = wasmparser::Parser::new(0);
    let mut module_types_builder = Default::default();
    let mut parsed_module = ModuleEnvironment::new(
        config,
        &mut validator,
        &mut module_types_builder,
    )
    .parse(parser, wasm, diagnostics)?;
    parsed_module.module.set_name_fallback(config.source_name.clone());
    if let Some(name_override) = config.override_name.as_ref() {
        parsed_module.module.set_name_override(name_override.clone());
    }
    let module_types = module_types_builder.finish();

    let mut module_state =
        ModuleTranslationState::new(&parsed_module.module, &module_types, vec![]);
    build_ir_module(&mut parsed_module, &module_types, &mut module_state, config, diagnostics)
}

/// Translate a valid Wasm core module binary into Miden IR component building
/// component imports for well-known Miden ABI functions
///
/// This is a temporary solution until we compile an account code as Wasm
/// component. To be able to do it we need wit-bindgen type re-mapping implemented first (see
/// https://github.com/0xPolygonMiden/compiler/issues/116)
pub fn translate_module_as_component(
    wasm: &[u8],
    config: &WasmTranslationConfig,
    diagnostics: &DiagnosticsHandler,
) -> WasmResult<miden_hir::Component> {
    let wasm_features = WasmFeatures::default();
    let mut validator = Validator::new_with_features(wasm_features);
    let parser = wasmparser::Parser::new(0);
    let mut module_types_builder = Default::default();
    let mut parsed_module = ModuleEnvironment::new(
        config,
        &mut validator,
        &mut module_types_builder,
    )
    .parse(parser, wasm, diagnostics)?;
    parsed_module.module.set_name_fallback(config.source_name.clone());
    if let Some(name_override) = config.override_name.as_ref() {
        parsed_module.module.set_name_override(name_override.clone());
    }
    let module_types = module_types_builder.finish();

    let mut module_state =
        ModuleTranslationState::new(&parsed_module.module, &module_types, vec![]);
    let module =
        build_ir_module(&mut parsed_module, &module_types, &mut module_state, config, diagnostics)?;
    let mut cb = miden_hir::ComponentBuilder::new(&diagnostics);
    let module_imports = module.imports();
    for import_module_id in module_imports.iter_module_names() {
        if let Some(imports) = module_imports.imported(import_module_id) {
            for ext_func in imports {
                if is_miden_intrinsics_module(ext_func.module.as_symbol()) {
                    // ignore intrinsics imports
                    continue;
                }
                let function_ty = miden_sdk_function_type(
                    ext_func.module.as_symbol(),
                    ext_func.function.as_symbol(),
                );
                let digest = *module_state.digest(ext_func).expect(
                    format!("failed to find MAST root hash for function {}", ext_func.function)
                        .as_str(),
                );
                let component_import = miden_hir::ComponentImport::MidenAbiImport(MidenAbiImport {
                    function_ty,
                    digest,
                });
                cb.add_import(*ext_func, component_import);
            }
        }
    }
    cb.add_module(module.into()).expect("module is already added");
    Ok(cb.build())
}

pub fn build_ir_module(
    parsed_module: &mut ParsedModule,
    module_types: &ModuleTypes,
    module_state: &mut ModuleTranslationState,
    _config: &WasmTranslationConfig,
    diagnostics: &DiagnosticsHandler,
) -> WasmResult<miden_hir::Module> {
    let name = parsed_module.module.name();
    let mut module_builder = ModuleBuilder::new(name.clone().as_str());
    build_globals(&parsed_module.module, &mut module_builder, diagnostics)?;
    build_data_segments(parsed_module, &mut module_builder, diagnostics)?;
    let mut func_translator = FuncTranslator::new();
    // Although this renders this parsed module invalid(without functiong
    // bodies), we don't support multiple module instances. Thus, this
    // ParseModule will not be used again to make another module instance.
    let func_body_inputs = mem::take(&mut parsed_module.function_body_inputs);
    for (defined_func_idx, body_data) in func_body_inputs {
        let func_index = &parsed_module.module.func_index(defined_func_idx);
        let func_type = &parsed_module.module.functions[*func_index];
        let func_name = &parsed_module.module.func_name(*func_index);
        let wasm_func_type = module_types[func_type.signature].clone();
        let ir_func_type = ir_func_type(&wasm_func_type)?;
        let sig = ir_func_sig(&ir_func_type, CallConv::SystemV, Linkage::External);
        let mut module_func_builder = module_builder.function(func_name.as_str(), sig.clone())?;
        let FunctionBodyData { validator, body } = body_data;
        let mut func_validator = validator.into_validator(Default::default());
        func_translator.translate_body(
            &body,
            &mut module_func_builder,
            module_state,
            &parsed_module.module,
            &module_types,
            diagnostics,
            &mut func_validator,
        )?;
        module_func_builder
            .build(diagnostics)
            .map_err(|_| WasmError::InvalidFunctionError)?;
    }
    let module = module_builder.build();
    Ok(*module)
}

fn build_globals(
    wasm_module: &Module,
    module_builder: &mut ModuleBuilder,
    diagnostics: &DiagnosticsHandler,
) -> Result<(), WasmError> {
    Ok(for (global_idx, global) in &wasm_module.globals {
        let global_name = wasm_module
            .name_section
            .globals_names
            .get(&global_idx)
            .cloned()
            .unwrap_or(Symbol::intern(format!("gv{}", global_idx.as_u32())));
        let global_init = wasm_module.try_global_initializer(global_idx, diagnostics)?;
        let init = ConstantData::from(global_init.to_le_bytes(&wasm_module, diagnostics)?);
        if let Err(e) = module_builder.declare_global_variable(
            global_name.as_str(),
            ir_type(global.ty.clone())?,
            Linkage::External,
            Some(init.clone()),
            SourceSpan::default(),
        ) {
            let message = format!(
                "Failed to declare global variable '{global_name}' with initializer '{init}' with \
                 error: {:?}",
                e
            );
            diagnostics
                .diagnostic(miden_diagnostics::Severity::Error)
                .with_message(message.clone())
                .emit();
            return Err(WasmError::Unexpected(message));
        }
    })
}

fn build_data_segments(
    translation: &ParsedModule,
    module_builder: &mut ModuleBuilder,
    diagnostics: &DiagnosticsHandler,
) -> Result<(), WasmError> {
    for (data_segment_idx, data_segment) in &translation.data_segments {
        let data_segment_name =
            translation.module.name_section.data_segment_names[&data_segment_idx].clone();
        let readonly = data_segment_name.as_str().contains(".rodata");
        let init = ConstantData::from(data_segment.data);
        let offset = data_segment.offset.as_i32(&translation.module, diagnostics)? as u32;
        let size = init.len() as u32;
        if let Err(e) = module_builder.declare_data_segment(offset, size, init, readonly) {
            let message = format!(
                "Failed to declare data segment '{data_segment_name}' with size '{size}' at \
                 '{offset}' with error: {:?}",
                e
            );
            diagnostics
                .diagnostic(miden_diagnostics::Severity::Error)
                .with_message(message.clone())
                .emit();
            return Err(WasmError::Unexpected(message));
        }
    }
    Ok(())
}
