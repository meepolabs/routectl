//! The literal `routectl` command a failure message tells the operator to
//! run again. It is printed only when it reproduces the failed invocation
//! exactly, against the same config file; otherwise the message says to
//! repeat the same command instead of printing a different one.

use std::borrow::Cow;
use std::path::Path;

use routectl_core::render_operator_text;

use super::seat::shell_word;

/// The variable the CLI's global `--config` flag also reads.
const CONFIG_ENV_VAR: &str = "ROUTECTL_CONFIG";
/// The file name under the config folder when no path is given.
const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// `config_path` when a rerun must name it with `--config`; `None` when a
/// bare `routectl`, with neither `--config` nor `ROUTECTL_CONFIG`, resolves
/// the same file. That default path is never echoed: it embeds the
/// operator's home directory, and the operator did not write it.
pub fn chosen_config_path(config_path: &Path) -> Option<&Path> {
    let is_default = std::env::var_os(CONFIG_ENV_VAR).is_none()
        && routectl_core::config_dir::config_dir()
            .is_some_and(|dir| dir.join(DEFAULT_CONFIG_FILE) == config_path);
    (!is_default).then_some(config_path)
}

/// A `routectl` command line built word by word. Each operator-written
/// value becomes one shell word holding exactly that value. A value with no
/// exact printable form (a control character, a path that is not UTF-8)
/// voids the whole command: dropping or rewriting it would name a different
/// seat, entry or file than the one the failed invocation used.
#[derive(Debug, Clone)]
#[must_use]
pub struct RerunCommand {
    words: Option<Vec<String>>,
}

impl RerunCommand {
    /// `routectl`, then `--config <path>` when `config_path` is given.
    pub fn routectl(config_path: Option<&Path>) -> Self {
        let command = Self {
            words: Some(vec!["routectl".to_string()]),
        };
        match config_path.map(Path::to_str) {
            None => command,
            Some(Some(path)) => command.word("--config").value(path),
            Some(None) => Self { words: None },
        }
    }

    /// A fixed word of the command: a subcommand or a flag name.
    pub fn word(mut self, word: &'static str) -> Self {
        if let Some(words) = &mut self.words {
            words.push(word.to_string());
        }
        self
    }

    /// An operator-written value, as one exact shell word.
    pub fn value(mut self, value: &str) -> Self {
        let is_exact = matches!(render_operator_text(value), Cow::Borrowed(_));
        self.words = self.words.filter(|_| is_exact).map(|mut words| {
            words.push(shell_word(value));
            words
        });
        self
    }

    /// `flag value` when `value` is given, else nothing.
    pub fn option(self, flag: &'static str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.word(flag).value(value),
            None => self,
        }
    }

    /// `flag` when `is_set`, else nothing.
    pub fn switch(self, flag: &'static str, is_set: bool) -> Self {
        if is_set { self.word(flag) } else { self }
    }

    /// The command line, or `None` when a value had no exact form.
    pub fn render(self) -> Option<String> {
        self.words.map(|words| words.join(" "))
    }
}

/// The recovery phrase: the exact command when there is one, else an
/// instruction to repeat the `same` command the operator already ran.
pub fn instruction(command: Option<String>, same: &str) -> String {
    command.map_or_else(
        || format!("re-run the same `{same}` command"),
        |command| format!("re-run `{command}`"),
    )
}
