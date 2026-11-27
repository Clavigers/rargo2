//! The one trait every rargo state machine implements.
//!
//! A state machine is a value that changes only through its `&mut self`
//! methods (its steps). Its `invariants()` says what must hold between any two
//! steps, and panics with what broke. Tests and `rargo-sim` call it after every
//! step through this trait, and `rargo-purity`'s self-check finds state
//! machines by their `impl StateMachine` (outside test code).

use std::fmt::Display;
use std::ops::Deref;

pub trait StateMachine {
    /// Panics, saying what broke, unless everything that must hold between
    /// two steps holds.
    fn invariants(&self);
}

impl<M: StateMachine + ?Sized> StateMachine for &M {
    fn invariants(&self) {
        (**self).invariants()
    }
}

impl<M: StateMachine + ?Sized> StateMachine for &mut M {
    fn invariants(&self) {
        (**self).invariants()
    }
}

impl<M: StateMachine + ?Sized> StateMachine for Box<M> {
    fn invariants(&self) {
        (**self).invariants()
    }
}

/// Checks every machine given, in order.
pub fn check_all<'a>(machines: impl IntoIterator<Item = &'a dyn StateMachine>) {
    for m in machines {
        m.invariants();
    }
}

/// Checks a machine's invariants and, if they break, panics with `at` (a
/// seed, a step) in front of what broke, so a failure says where it happened.
pub fn check(machine: &(impl StateMachine + ?Sized), at: impl Display) {
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| machine.invariants())) {
        let what = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        panic!("{at}: {what}");
    }
}

/// A machine whose invariants are checked when it is wrapped and after every
/// step taken through [`Checked::step`]. Reading goes through `Deref`; there
/// is no `DerefMut`, so a test cannot step it without the check.
pub struct Checked<M: StateMachine>(M);

impl<M: StateMachine> Checked<M> {
    pub fn new(machine: M) -> Checked<M> {
        machine.invariants();
        Checked(machine)
    }

    /// Takes one step and checks the invariants after it.
    pub fn step<R>(&mut self, f: impl FnOnce(&mut M) -> R) -> R {
        let r = f(&mut self.0);
        self.0.invariants();
        r
    }

    pub fn into_inner(self) -> M {
        self.0
    }
}

impl<M: StateMachine> Deref for Checked<M> {
    type Target = M;
    fn deref(&self) -> &M {
        &self.0
    }
}

/// Registers separate tests for independent batches of a deterministic scenario.
#[macro_export]
macro_rules! test_batches {
    ($run:path; $($name:ident: $batch:expr),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                $run($batch);
            }
        )+
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u8);

    impl StateMachine for Counter {
        fn invariants(&self) {
            assert!(self.0 < 3, "the counter is {}, above its limit of 2", self.0);
        }
    }

    #[test]
    fn a_checked_machine_is_checked_after_every_step() {
        let mut c = Checked::new(Counter(0));
        assert_eq!(c.step(|c| {
            c.0 += 1;
            c.0
        }), 1);
        c.step(|c| c.0 += 1);
        assert_eq!((*c).0, 2);
        let broke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.step(|c| c.0 += 1)));
        let text = broke.unwrap_err().downcast::<String>().unwrap();
        assert_eq!(*text, "the counter is 3, above its limit of 2");
    }

    #[test]
    fn machines_are_checked_through_the_trait_object() {
        let a = Counter(1);
        let b = Box::new(Counter(2));
        check_all([&a as &dyn StateMachine, &b]);
        assert!(std::panic::catch_unwind(|| Checked::new(Counter(5))).is_err());
    }

    #[test]
    fn a_broken_check_says_where_it_broke() {
        check(&Counter(2), "seed 1");
        let broke = std::panic::catch_unwind(|| check(&Counter(4), format_args!("seed {} step {}", 7, 3)));
        let text = broke.unwrap_err().downcast::<String>().unwrap();
        assert_eq!(*text, "seed 7 step 3: the counter is 4, above its limit of 2");
    }
}
