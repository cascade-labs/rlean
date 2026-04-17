use lean_core::{DateTime, Price, Symbol};

pub trait BenchmarkProvider: Send + Sync {
    fn evaluate(&self, time: DateTime) -> Price;
    fn symbol(&self) -> Option<&Symbol> {
        None
    }
}

pub struct FunctionBenchmark {
    func: Box<dyn Fn(DateTime) -> Price + Send + Sync>,
}

impl FunctionBenchmark {
    pub fn new(f: impl Fn(DateTime) -> Price + Send + Sync + 'static) -> Self {
        FunctionBenchmark { func: Box::new(f) }
    }
}

impl BenchmarkProvider for FunctionBenchmark {
    fn evaluate(&self, time: DateTime) -> Price {
        (self.func)(time)
    }
}
