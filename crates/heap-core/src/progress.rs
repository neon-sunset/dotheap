use std::sync::Mutex;

#[derive(Clone, Copy)]
pub(crate) struct Progress<'a> {
    update: &'a (dyn Fn(&str, f64) + Sync),
    name: &'static str,
    start: f64,
    end: f64,
}

impl<'a> Progress<'a> {
    pub fn new(update: &'a (dyn Fn(&str, f64) + Sync)) -> Self {
        Self {
            update,
            name: "",
            start: 0.0,
            end: 1.0,
        }
    }

    pub fn none() -> Self {
        Self::new(&|_, _| {})
    }

    pub fn portion(self, start: f64, end: f64) -> Self {
        let span = self.end - self.start;
        Self {
            start: self.start + span * start,
            end: self.start + span * end,
            ..self
        }
    }

    pub fn named(self, name: &'static str) -> Self {
        Self { name, ..self }
    }

    pub fn phases<const N: usize>(self, phases: [(&'static str, f64); N]) -> [Self; N] {
        let total: f64 = phases.iter().map(|(_, weight)| weight).sum();
        let mut start = 0.0;
        phases.map(|(name, weight)| {
            let end = start + weight / total;
            let phase = self.portion(start, end).named(name);
            start = end;
            phase
        })
    }

    pub fn report(self, fraction: f64) {
        (self.update)(
            self.name,
            self.start + (self.end - self.start) * fraction.clamp(0.0, 1.0),
        );
    }

    pub fn items(self, done: usize, total: usize) {
        self.report(if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        });
    }

    pub fn counter(self, total: u64) -> impl Fn(u64) + Sync + 'a {
        let done = Mutex::new(0u64);
        move |count| {
            let mut done = done.lock().unwrap();
            *done += count;
            self.report(if total == 0 {
                1.0
            } else {
                *done as f64 / total as f64
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_progress_is_weighted_and_single_phases_use_the_whole_range() {
        let values = Mutex::new(Vec::new());
        let update = |_: &str, fraction: f64| values.lock().unwrap().push(fraction);
        let progress = Progress::new(&update);
        let [only] = progress.phases([("Cache", 7.0)]);
        only.report(0.5);
        let [first, second] = progress.phases([("Read", 1.0), ("Analyze", 3.0)]);
        first.report(0.5);
        second.report(0.5);
        second.report(1.0);
        assert_eq!(*values.lock().unwrap(), [0.5, 0.125, 0.625, 1.0]);
    }
}
