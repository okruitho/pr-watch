use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<Output> {
    run_env(cmd, args, cwd, &[])
}

pub fn run_env(cmd: &str, args: &[&str], cwd: Option<&Path>, env: &[(&str, &OsStr)]) -> Result<Output> {
    let mut c = Command::new(cmd);
    c.args(args).envs(env.iter().copied()).stdin(Stdio::null());
    if let Some(cwd) = cwd {
        c.current_dir(cwd);
    }
    let out = c.output().map_err(|e| anyhow::anyhow!("running {cmd}: {e}"))?;
    Ok(Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

pub fn run_ok(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    run_ok_env(cmd, args, cwd, &[])
}

pub fn run_ok_env(cmd: &str, args: &[&str], cwd: Option<&Path>, env: &[(&str, &OsStr)]) -> Result<String> {
    let out = run_env(cmd, args, cwd, env)?;
    if out.code != 0 {
        let err = out.stderr.trim();
        bail!(
            "{cmd} {}: {}",
            args.join(" "),
            if err.is_empty() {
                format!("exit {}", out.code)
            } else {
                err.to_string()
            }
        );
    }
    Ok(out.stdout)
}
