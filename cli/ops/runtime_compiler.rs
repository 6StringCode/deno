// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::import_map::ImportMap;
use crate::module_graph::BundleType;
use crate::module_graph::EmitOptions;
use crate::module_graph::GraphBuilder;
use crate::program_state::ProgramState;
use crate::specifier_handler::FetchHandler;
use crate::specifier_handler::MemoryHandler;
use crate::specifier_handler::SpecifierHandler;

use deno_core::error::generic_error;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::error::Context;
use deno_core::parking_lot::Mutex;
use deno_core::resolve_url_or_path;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::OpState;
use deno_runtime::permissions::Permissions;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub fn init(rt: &mut deno_core::JsRuntime) {
  super::reg_async(rt, "op_emit", op_emit);
}

#[derive(Debug, Deserialize)]
enum RuntimeBundleType {
  #[serde(rename = "module")]
  Module,
  #[serde(rename = "classic")]
  Classic,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmitArgs {
  bundle: Option<RuntimeBundleType>,
  check: Option<bool>,
  compiler_options: Option<HashMap<String, Value>>,
  import_map: Option<Value>,
  import_map_path: Option<String>,
  root_specifier: String,
  sources: Option<HashMap<String, Arc<String>>>,
}

async fn op_emit(
  state: Rc<RefCell<OpState>>,
  args: Value,
  _: (),
) -> Result<Value, AnyError> {
  deno_runtime::ops::check_unstable2(&state, "Deno.emit");
  let args: EmitArgs = serde_json::from_value(args)?;
  let root_specifier = args.root_specifier;
  let program_state = state.borrow().borrow::<Arc<ProgramState>>().clone();
  let mut runtime_permissions = {
    let state = state.borrow();
    state.borrow::<Permissions>().clone()
  };
  // when we are actually resolving modules without provided sources, we should
  // treat the root module as a dynamic import so that runtime permissions are
  // applied.
  let handler: Arc<Mutex<dyn SpecifierHandler>> =
    if let Some(sources) = args.sources {
      Arc::new(Mutex::new(MemoryHandler::new(sources)))
    } else {
      Arc::new(Mutex::new(FetchHandler::new(
        &program_state,
        runtime_permissions.clone(),
        runtime_permissions.clone(),
      )?))
    };
  let maybe_import_map = if let Some(import_map_str) = args.import_map_path {
    let import_map_specifier = resolve_url_or_path(&import_map_str)
      .context(format!("Bad URL (\"{}\") for import map.", import_map_str))?;
    let import_map = if let Some(value) = args.import_map {
      ImportMap::from_json(import_map_specifier.as_str(), &value.to_string())?
    } else {
      let file = program_state
        .file_fetcher
        .fetch(&import_map_specifier, &mut runtime_permissions)
        .await
        .map_err(|e| {
          generic_error(format!(
            "Unable to load '{}' import map: {}",
            import_map_specifier, e
          ))
        })?;
      ImportMap::from_json(import_map_specifier.as_str(), &file.source)?
    };
    Some(import_map)
  } else if args.import_map.is_some() {
    return Err(generic_error("An importMap was specified, but no importMapPath was provided, which is required."));
  } else {
    None
  };
  let mut builder = GraphBuilder::new(handler, maybe_import_map, None);
  let root_specifier = resolve_url_or_path(&root_specifier)?;
  builder.add(&root_specifier, false).await.map_err(|_| {
    type_error(format!(
      "Unable to handle the given specifier: {}",
      &root_specifier
    ))
  })?;
  builder
    .analyze_compiler_options(&args.compiler_options)
    .await?;
  let bundle_type = match args.bundle {
    Some(RuntimeBundleType::Module) => BundleType::Module,
    Some(RuntimeBundleType::Classic) => BundleType::Classic,
    None => BundleType::None,
  };
  let graph = builder.get_graph();
  let debug = program_state.flags.log_level == Some(log::Level::Debug);
  let graph_errors = graph.get_errors();
  let (files, mut result_info) = graph.emit(EmitOptions {
    bundle_type,
    check: args.check.unwrap_or(true),
    debug,
    maybe_user_config: args.compiler_options,
  })?;
  result_info.diagnostics.extend_graph_errors(graph_errors);

  Ok(json!({
    "diagnostics": result_info.diagnostics,
    "files": files,
    "ignoredOptions": result_info.maybe_ignored_options,
    "stats": result_info.stats,
  }))
}
