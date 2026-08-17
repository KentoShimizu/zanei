//! Focus-generation state shared by native Accessibility observers.

pub(crate) struct FocusedTarget<T> {
    generation: u64,
    current: Option<T>,
}

impl<T> FocusedTarget<T> {
    pub(crate) fn new() -> Self {
        Self {
            generation: 0,
            current: None,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn next_generation(&self) -> u64 {
        self.generation.saturating_add(1)
    }

    pub(crate) fn current(&self) -> Option<&T> {
        self.current.as_ref()
    }

    pub(crate) fn current_mut(&mut self) -> Option<&mut T> {
        self.current.as_mut()
    }

    pub(crate) fn transition<E>(
        &mut self,
        registered: Result<Option<T>, E>,
    ) -> Result<Option<T>, (Option<T>, E)> {
        self.generation = self.next_generation();
        let previous = self.current.take();
        match registered {
            Ok(current) => {
                self.current = current;
                Ok(previous)
            }
            Err(error) => Err((previous, error)),
        }
    }
}
