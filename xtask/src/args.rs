//! Minimal command-line parsing: `<command> [--flag] [--key value | --key=value]...`.

use anyhow::{Result, bail};

pub struct Args {
    rest: Vec<String>,
}

impl Args {
    pub fn from_env() -> Self {
        Self {
            rest: std::env::args().skip(1).collect(),
        }
    }

    /// Removes and returns the first argument (the subcommand).
    pub fn command(&mut self) -> Option<String> {
        if self.rest.is_empty() {
            None
        } else {
            Some(self.rest.remove(0))
        }
    }

    /// Removes `name` if present.
    pub fn flag(&mut self, name: &str) -> bool {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }

    /// Removes `name value` or `name=value` and returns the value.
    pub fn value(&mut self, name: &str) -> Result<Option<String>> {
        let prefix = format!("{name}=");
        if let Some(i) = self.rest.iter().position(|a| a.starts_with(&prefix)) {
            let arg = self.rest.remove(i);
            return Ok(Some(arg[prefix.len()..].to_owned()));
        }
        let Some(i) = self.rest.iter().position(|a| a == name) else {
            return Ok(None);
        };
        if i + 1 >= self.rest.len() || self.rest[i + 1].starts_with("--") {
            bail!("{name} needs a value");
        }
        let value = self.rest.remove(i + 1);
        self.rest.remove(i);
        Ok(Some(value))
    }

    /// Fails if any argument was not consumed.
    pub fn finish(self) -> Result<()> {
        if !self.rest.is_empty() {
            bail!("unexpected argument(s): {}", self.rest.join(" "));
        }
        Ok(())
    }
}
