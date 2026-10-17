//! Fetch progress on stderr.

use std::{
    io::IsTerminal,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use actup_core::fetch::{Outcome, Progress};
use indicatif::{ProgressBar, ProgressDrawTarget};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    total: usize,
    cache: usize,
    git: usize,
    api: usize,
    provider: usize,
    failed: usize,
}

impl Counts {
    const fn done(&self) -> usize {
        self.cache + self.git + self.api + self.provider + self.failed
    }

    fn message(&self) -> String {
        format!(
            "resolving {} repos ({} done: cache {}, git {}, api {}, provider {}, failed {})",
            self.total,
            self.done(),
            self.cache,
            self.git,
            self.api,
            self.provider,
            self.failed
        )
    }
}

/// A one-line spinner on stderr, drawn only when stderr is a terminal.
pub struct Spinner {
    bar: ProgressBar,
    counts: Mutex<Counts>,
}

impl Spinner {
    /// A spinner on stderr when it is a terminal, else a hidden one.
    #[must_use]
    pub fn stderr() -> Self {
        Self::for_terminal(std::io::stderr().is_terminal())
    }

    fn for_terminal(is_terminal: bool) -> Self {
        let spinner = Self::with_target(if is_terminal {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        });
        if is_terminal {
            spinner.bar.enable_steady_tick(Duration::from_millis(120));
        }
        spinner
    }

    fn with_target(target: ProgressDrawTarget) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_draw_target(target);
        Self {
            bar,
            counts: Mutex::new(Counts::default()),
        }
    }

    /// Clear the spinner line.
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }

    fn update(&self, change: impl FnOnce(&mut Counts)) {
        let message = {
            let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
            change(&mut counts);
            counts.message()
        };
        self.bar.set_message(message);
        self.bar.tick();
    }
}

impl Progress for Spinner {
    fn start(&self, repos: usize) {
        self.update(|counts| counts.total = repos);
    }

    fn finished(&self, outcome: Outcome) {
        self.update(|counts| match outcome {
            Outcome::Cache => counts.cache += 1,
            Outcome::Git => counts.git += 1,
            Outcome::Api => counts.api += 1,
            Outcome::Provider => counts.provider += 1,
            Outcome::Failed => counts.failed += 1,
        });
    }
}

#[cfg(test)]
mod tests {
    use indicatif::{InMemoryTerm, TermLike};

    use super::*;

    #[test]
    fn spinner_counts_every_outcome_on_one_line() {
        let term = InMemoryTerm::new(4, 120);
        let spinner = Spinner::with_target(ProgressDrawTarget::term_like(Box::new(term.clone())));
        spinner.start(6);
        for outcome in [
            Outcome::Cache,
            Outcome::Git,
            Outcome::Git,
            Outcome::Api,
            Outcome::Provider,
            Outcome::Failed,
        ] {
            spinner.finished(outcome);
        }
        let screen = term.contents();
        assert!(
            screen.ends_with(
                "resolving 6 repos (6 done: cache 1, git 2, api 1, provider 1, failed 1)"
            ),
            "{screen:?}"
        );
        assert_eq!(screen.lines().count(), 1, "{screen:?}");
        assert_eq!(term.height(), 4);
        spinner.finish();
        assert_eq!(term.contents(), "");
    }

    #[test]
    fn spinner_hides_without_a_terminal() {
        let hidden = Spinner::for_terminal(false);
        assert!(hidden.bar.is_hidden());
        hidden.start(1);
        hidden.finished(Outcome::Git);
        hidden.finish();

        let terminal = Spinner::for_terminal(true);
        assert_eq!(terminal.bar.is_hidden(), !std::io::stderr().is_terminal());
        terminal.finish();
    }
}
