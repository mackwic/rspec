//! Runners are responsible for executing a test suite's examples.

mod configuration;
mod observer;

pub use runner::configuration::*;
pub use runner::observer::*;

use std::borrow::Borrow;
use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::panic;
use std::process;
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use block::Block;
use block::Suite;
use block::Context;
use block::Example;
use report::{Report, BlockReport};
use report::ContextReport;
use report::SuiteReport;
use report::ExampleReport;
use visitor::TestSuiteVisitor;

/// Runner for executing a test suite's examples.
pub struct Runner {
    configuration: configuration::Configuration,
    observers: Vec<Arc<RunnerObserver>>,
    should_exit: Mutex<Cell<bool>>,
}

impl Runner {
    pub fn new(configuration: Configuration, observers: Vec<Arc<RunnerObserver>>) -> Runner {
        Runner {
            configuration: configuration,
            observers: observers,
            should_exit: Mutex::new(Cell::new(false)),
        }
    }
}

impl Runner {
    pub fn run<T>(&self, suite: Suite<T>) -> SuiteReport
    where
        T: Clone + Send + Sync + ::std::fmt::Debug,
    {
        let mut environment = suite.environment.clone();
        self.prepare_before_run();
        let report = self.visit(&suite, &mut environment);
        self.clean_after_run();
        if let Ok(mut mutex_guard) = self.should_exit.lock() {
            *mutex_guard.deref_mut().get_mut() |= report.is_failure();
        }
        report
    }

    fn broadcast<F>(&self, mut f: F)
    where
        F: FnMut(&RunnerObserver),
    {
        for observer in &self.observers {
            f(observer.borrow());
        }
    }

    fn wrap_all<T, U, F>(&self, context: &Context<T>, environment: &mut T, f: F) -> U
    where
        F: Fn(&mut T) -> U,
    {
        for before_function in context.before_all.iter() {
            before_function(environment);
        }
        let result = f(environment);
        for after_function in context.after_all.iter() {
            after_function(environment);
        }
        result
    }

    fn wrap_each<T, U, F>(&self, context: &Context<T>, environment: &mut T, f: F) -> U
    where
        F: Fn(&mut T) -> U,
    {
        for before_function in context.before_each.iter() {
            before_function(environment);
        }
        let result = f(environment);
        for after_function in context.after_each.iter() {
            after_function(environment);
        }
        result
    }

    fn evaluate_blocks_parallel<T>(&self, context: &Context<T>, environment: &T) -> Vec<BlockReport>
    where
        T: Clone + Send + Sync + ::std::fmt::Debug,
    {
        context
            .blocks
            .par_iter()
            .map(|block| self.evaluate_block(block, context, environment))
            .collect()
    }

    fn evaluate_blocks_serial<T>(&self, context: &Context<T>, environment: &T) -> Vec<BlockReport>
    where
        T: Clone + Send + Sync + ::std::fmt::Debug,
    {
        context
            .blocks
            .iter()
            .map(|block| self.evaluate_block(block, context, environment))
            .collect()
    }

    fn evaluate_block<T>(
        &self,
        block: &Block<T>,
        context: &Context<T>,
        environment: &T,
    ) -> BlockReport
    where
        T: Clone + Send + Sync + ::std::fmt::Debug,
    {
        let mut environment = environment.clone();
        self.wrap_each(context, &mut environment, |environment| {
            self.visit(block, environment)
        })
    }

    fn prepare_before_run(&self) {
        panic::set_hook(Box::new(|_panic_info| {
            // XXX panics already catched at the test call site, don't output the trace in stdout
        }));
    }

    fn clean_after_run(&self) {
        // XXX reset panic hook back to default hook:
        let _ = panic::take_hook();
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let should_exit = if let Ok(mutex_guard) = self.should_exit.lock() {
            mutex_guard.deref().get()
        } else {
            false
        };

        if self.configuration.exit_on_failure && should_exit {
            // XXX Cargo test failure returns 101.
            //
            // > "We use 101 as the standard failure exit code because it's something unique
            // > that the test runner can check for in run-fail tests (as opposed to something
            // > like 1, which everybody uses). I don't expect this behavior can ever change.
            // > This behavior probably dates to before 2013,
            // > all the way back to the creation of compiletest." – @brson

            process::exit(101);
        }
    }
}


impl<T> TestSuiteVisitor<Suite<T>> for Runner
where
    T: Clone + Send + Sync + ::std::fmt::Debug,
{
    type Environment = T;
    type Output = SuiteReport;

    fn visit(&self, suite: &Suite<T>, environment: &mut Self::Environment) -> Self::Output {
        self.broadcast(|handler| handler.enter_suite(&suite.header));
        let report = SuiteReport::new(
            suite.header.clone(),
            self.visit(&suite.context, environment),
        );
        self.broadcast(|handler| handler.exit_suite(&suite.header, &report));
        report
    }
}

impl<T> TestSuiteVisitor<Block<T>> for Runner
where
    T: Clone + Send + Sync + ::std::fmt::Debug,
{
    type Environment = T;
    type Output = BlockReport;

    fn visit(&self, member: &Block<T>, environment: &mut Self::Environment) -> Self::Output {
        match member {
            &Block::Example(ref example) => {
                let header = example.header.clone();
                let report = self.visit(example, environment);
                BlockReport::Example(header, report)
            }
            &Block::Context(ref context) => {
                let header = context.header.clone();
                let report = self.visit(context, &mut environment.clone());
                BlockReport::Context(header, report)
            }
        }
    }
}

impl<T> TestSuiteVisitor<Context<T>> for Runner
where
    T: Clone + Send + Sync + ::std::fmt::Debug,
{
    type Environment = T;
    type Output = ContextReport;

    fn visit(&self, context: &Context<T>, environment: &mut Self::Environment) -> Self::Output {
        if let Some(ref header) = context.header {
            self.broadcast(|handler| handler.enter_context(&header));
        }
        let reports: Vec<_> =
            self.wrap_all(context, environment, |environment| if self.configuration
                .parallel
            {
                self.evaluate_blocks_parallel(context, environment)
            } else {
                self.evaluate_blocks_serial(context, environment)
            });
        let report = ContextReport::new(reports);
        if let Some(ref header) = context.header {
            self.broadcast(|handler| handler.exit_context(&header, &report));
        }
        report
    }
}

impl<T> TestSuiteVisitor<Example<T>> for Runner
where
    T: Clone + Send + Sync + ::std::fmt::Debug,
{
    type Environment = T;
    type Output = ExampleReport;

    fn visit(&self, example: &Example<T>, environment: &mut Self::Environment) -> Self::Output {
        self.broadcast(|handler| handler.enter_example(&example.header));
        let function = &example.function;
        let report = function(environment);
        self.broadcast(|handler| handler.exit_example(&example.header, &report));
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod runner {
        use super::*;

        #[test]
        fn it_can_be_instanciated() {
            // arrange
            let _ = Runner::new(Configuration::default(), vec!());
            // act
            // assert
        }

        mod broadcast {
            use super::*;

            use header::*;
            use std::sync::atomic::*;

            // XXX blank impl for stubbing
            impl RunnerObserver for () {}

            #[test]
            fn it_call_the_closure() {
                // arrange
                let spy = Arc::new(());
                let runner = Runner::new(Configuration::default(), vec!(spy));
                let has_been_called = AtomicBool::new(false);
                // act
                runner.broadcast(|_| has_been_called.store(true, Ordering::SeqCst));
                // assert
                assert_eq!(true, has_been_called.load(Ordering::SeqCst));
            }

            #[test]
            fn it_call_it_once_per_observer() {
                // arrange
                let spy1 = Arc::new(());
                let spy2 = Arc::new(());
                let runner = Runner::new(Configuration::default(), vec![spy1, spy2]);
                let call_times = AtomicUsize::new(0);
                // act
                runner.broadcast(|_| { call_times.fetch_add(1, Ordering::SeqCst); });
                // assert
                assert_eq!(2, call_times.load(Ordering::SeqCst))
            }

            struct ObserverStub {
                events: Mutex<Vec<(&'static str, SuiteHeader)>>,
            }
            impl ObserverStub {
                fn new() -> Self {
                    ObserverStub { events: Mutex::new(vec!()) }
                }
            }

            // XXX stub implem
            impl RunnerObserver for ObserverStub {
                fn enter_suite(&self, header: &SuiteHeader) {
                    let mut vec = self.events.lock().unwrap();
                    (*vec).push(("enter_suite", header.clone()));
                }
            }

            #[test]
            fn it_gives_the_observer_as_callback_argument() {
                // arrange
                let spy1 = Arc::new(ObserverStub::new());
                let expected = SuiteHeader::new(SuiteLabel::Describe, "hello");
                let runner = Runner::new(Configuration::default(), vec![spy1.clone()]);
                // act
                runner.broadcast(|observer| observer.enter_suite(&expected.clone()));
                // assert
                let lock = spy1.events.lock().expect("no dangling threads");
                let res = (*lock).get(0).expect("to have been called once");
                assert_eq!(&("enter_suite", expected), res);
            }
        }
    }

}