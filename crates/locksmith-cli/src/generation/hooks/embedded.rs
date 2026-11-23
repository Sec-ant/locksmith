use llrt_modules::{
    module_builder::ModuleBuilder,
    package::{
        loader::PackageLoader,
        resolver::{PackageResolver, require_resolve},
    },
};
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Function, Promise, Value as JsValue,
};
use serde_json::{Value, json};

use crate::Result;

pub(super) struct Engine {
    context: AsyncContext,
    executor: tokio::runtime::Runtime,
}

fn resolve(ctx: Ctx<'_>, name: String, parent: String) -> rquickjs::Result<String> {
    let resolved = require_resolve(&ctx, &name, &parent, None, false)?;
    let path = std::fs::canonicalize(resolved.as_ref())?;
    let path = path.to_string_lossy();
    #[cfg(windows)]
    let path = match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!("//{}", share.replace('\\', "/")),
        None => path
            .strip_prefix(r"\\?\")
            .unwrap_or(&path)
            .replace('\\', "/"),
    };
    Ok(path.to_string())
}

impl Engine {
    pub fn new() -> Result<Self> {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        let runtime = AsyncRuntime::new()?;
        let (modules, loader, globals) = ModuleBuilder::default().build();
        let context = executor.block_on(async {
            runtime
                .set_loader((modules, PackageResolver), (loader, PackageLoader))
                .await;
            AsyncContext::full(&runtime).await
        })?;
        let environment = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect::<std::collections::BTreeMap<_, _>>();
        let cwd = std::env::current_dir()?;
        executor.block_on(context.with(|ctx| -> Result<()> {
            let setup = || -> rquickjs::Result<()> {
                globals.attach(&ctx)?;
                let global = ctx.globals();
                global.set(
                    "hookLog",
                    Function::new(ctx.clone(), |s: String| eprintln!("{s}"))?,
                )?;
                global.set("hookResolve", Function::new(ctx.clone(), resolve)?)?;
                global.set(
                    "hookRead",
                    Function::new(ctx.clone(), |path: String| {
                        Ok::<_, rquickjs::Error>(std::fs::read_to_string(path)?)
                    })?,
                )?;
                global.set(
                    "hookEnvironment",
                    ctx.json_parse(
                        serde_json::to_vec(&json!({"env":environment,"cwd":cwd}))
                            .expect("JSON inputs"),
                    )?,
                )?;
                ctx.eval::<(), _>(format!(
                    "(function(module) {{\n{}\n{}\n}})()",
                    super::HOOKS,
                    include_str!("embedded.cjs")
                ))
            };
            setup()
                .catch(&ctx)
                .map_err(|e| format!("embedded pnpmfile: {e}").into())
        }))?;
        Ok(Self { context, executor })
    }
    pub fn call(&self, request: &impl serde::Serialize) -> Result<Value> {
        let input = serde_json::to_vec(request)?;
        let output = self
            .executor
            .block_on(rquickjs::async_with!(self.context => |ctx| {
                let call = async {
                    let f: Function = ctx.globals().get("dispatchHook")?;
                    let promise: Promise = f.call((ctx.json_parse(input)?,))?;
                    let value: JsValue = promise.into_future().await?;
                    ctx.json_stringify(value)?
                        .ok_or(rquickjs::Error::Unknown)?
                        .to_string()
                };
                call.await
                    .catch(&ctx)
                    .map_err(|e| format!("embedded pnpmfile: {e}"))
            }))?;
        Ok(serde_json::from_str(&output)?)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let cleanup = self.executor.block_on(self.context.with(|ctx| {
            let release: Function = ctx.globals().get("releaseHookTimers")?;
            release.call::<_, ()>(())
        }));
        if let Err(error) = cleanup {
            eprintln!("embedded pnpmfile timer cleanup failed: {error}");
        }
    }
}
