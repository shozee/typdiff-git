use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::process::{Command, Stdio};

pub fn require(program: &str) -> Result<()> {
    let status = Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();
    match status {
        Ok(_) => Ok(()),
        Err(_) => bail!("required executable not found in PATH: {program}"),
    }
}

pub fn run<I, S>(program: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new(program).args(args).status().with_context(|| format!("running {program}"))?;
    if !status.success() { bail!("{program} exited with status {status}"); }
    Ok(())
}
