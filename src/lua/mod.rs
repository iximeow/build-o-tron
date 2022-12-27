use crate::RunnerClient;
use crate::RunningJob;

use rlua::prelude::*;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

pub const DEFAULT_RUST_GOODFILE: &'static [u8] = include_bytes!("../../config/goodfiles/rust.lua");

pub struct BuildEnv {
    lua: Lua,
    job: Arc<Mutex<RunningJob>>,
}

impl BuildEnv {
    pub fn new(job: &Arc<Mutex<RunningJob>>) -> Self {
        let env = BuildEnv {
            lua: Lua::new(),
            job: Arc::clone(job),
        };
        env.lua.context(|lua_ctx| {
            env.define_env(lua_ctx)
        }).expect("can define context");
        env
    }

    fn define_env(&self, lua_ctx: LuaContext) -> Result<(), String> {
        let hello = lua_ctx.create_function(|_, ()| {
            eprintln!("hello from lua!!!");
            Ok(())
        })
            .map_err(|e| format!("problem defining build function: {:?}", e))?;
        let job_ref = Arc::clone(&self.job);
        let build = lua_ctx.create_function(move |_, (command, params): (LuaValue, LuaValue)| {
            let job_ref: Arc<Mutex<RunningJob>> = Arc::clone(&job_ref);
            let args = match command {
                LuaValue::Table(table) => {
                    let len = table.len().expect("command table has a length");
                    let mut command_args = Vec::new();
                    for i in 0..len {
                        let value = table.get(i + 1).expect("command arg is gettble");
                        match value {
                            LuaValue::String(s) => {
                                command_args.push(s.to_str().unwrap().to_owned());
                            },
                            other => {
                                return Err(LuaError::RuntimeError(format!("argument {} was not a string, was {:?}", i, other)));
                            }
                        };
                    }

                    command_args
                },
                other => {
                    return Err(LuaError::RuntimeError(format!("argument 1 was not a table: {:?}", other)));
                }
            };
            eprintln!("args: {:?}", args);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                job_ref.lock().unwrap().run_command(&args).await
                    .map_err(|e| LuaError::RuntimeError(format!("run_command error: {:?}", e)))
            })
        })
            .map_err(|e| format!("problem defining build function: {:?}", e))?;

        let job_ref = Arc::clone(&self.job);
        let metric = lua_ctx.create_function(move |_, (name, value): (String, String)| {
            let job_ref: Arc<Mutex<RunningJob>> = Arc::clone(&job_ref);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                job_ref.lock().unwrap().send_metric(&name, value).await
                    .map_err(|e| LuaError::RuntimeError(format!("send_metric error: {:?}", e)))
            })
        })
            .map_err(|e| format!("problem defining metric function: {:?}", e))?;

        let job_ref = Arc::clone(&self.job);
        let artifact = lua_ctx.create_function(move |_, (name, path): (String, String)| {
            let job_ref: Arc<Mutex<RunningJob>> = Arc::clone(&job_ref);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let artifact = job_ref.lock().unwrap().create_artifact(&name, &format!("{} (from {})", name, path)).await
                    .map_err(|e| LuaError::RuntimeError(format!("create_artifact error: {:?}", e)))
                    .unwrap();
                crate::forward_data(tokio::fs::File::open(path).await.unwrap(), artifact).await
                    .map_err(|e| LuaError::RuntimeError(format!("failed uploading data for {}: {:?}", name, e)))
            })
        })
            .map_err(|e| format!("problem defining metric function: {:?}", e))?;

        let error = lua_ctx.create_function(move |_, msg: String| {
            Err::<(), LuaError>(LuaError::RuntimeError(format!("explicit error: {}", msg)))
        }).unwrap();

        let path_has_cmd = lua_ctx.create_function(move |_, name: String| {
            Ok(std::process::Command::new("which")
                .arg(name)
                .status()
                .map_err(|e| LuaError::RuntimeError(format!("could not fork which? {:?}", e)))?
                .success())
        }).unwrap();

        let size_of_file = lua_ctx.create_function(move |_, name: String| {
            Ok(std::fs::metadata(&name)
                .map_err(|e| LuaError::RuntimeError(format!("could not stat {:?}", name)))?
                .len())
        }).unwrap();

        let build_environment = lua_ctx.create_table_from(
            vec![
                ("has", path_has_cmd),
                ("size", size_of_file),
            ]
        ).unwrap();

        let build_functions = lua_ctx.create_table_from(
            vec![
                ("hello", hello),
                ("run", build),
                ("metric", metric),
                ("error", error),
                ("artifact", artifact),
            ]
        ).unwrap();
        build_functions.set("environment", build_environment).unwrap();
        let globals = lua_ctx.globals();
        globals.set("Build", build_functions);
        Ok(())
    }

    pub async fn run_build(self, script: &[u8]) -> Result<(), LuaError> {
        let script = script.to_vec();
        let res: Result<(), LuaError> = std::thread::spawn(move || {
            self.lua.context(|lua_ctx| {
                lua_ctx.load(&script)
                    .set_name("goodfile")?
                    .exec()
            })
        }).join().unwrap();
        eprintln!("lua res: {:?}", res);
        res
    }
}