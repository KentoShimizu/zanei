use crate::focused_field::FieldClass;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::ffi::ax) enum ValueNotificationRegistration {
    #[default]
    Unregistered,
    Active,
    Stale,
}

pub(in crate::ffi::ax) enum RegistrationError<E> {
    Register(E),
    Unregister(E),
}

impl ValueNotificationRegistration {
    pub(in crate::ffi::ax) fn reconcile<E>(
        &mut self,
        field_class: FieldClass,
        register: impl FnOnce() -> Result<(), E>,
        unregister: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), RegistrationError<E>> {
        if value_notification_eligible(field_class) {
            match self {
                Self::Unregistered => match register() {
                    Ok(()) => *self = Self::Active,
                    Err(error) => return Err(RegistrationError::Register(error)),
                },
                Self::Stale => *self = Self::Active,
                Self::Active => {}
            }
        } else if *self == Self::Active {
            match unregister() {
                Ok(()) => *self = Self::Unregistered,
                Err(error) => {
                    *self = Self::Stale;
                    return Err(RegistrationError::Unregister(error));
                }
            }
        }
        Ok(())
    }

    pub(in crate::ffi::ax) fn clear<E>(
        &mut self,
        unregister: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.needs_cleanup() {
            return Ok(());
        }
        match unregister() {
            Ok(()) => {
                *self = Self::Unregistered;
                Ok(())
            }
            Err(error) => {
                *self = Self::Stale;
                Err(error)
            }
        }
    }

    pub(in crate::ffi::ax) const fn accepts_delivery(self) -> bool {
        matches!(self, Self::Active)
    }

    pub(in crate::ffi::ax) const fn needs_cleanup(self) -> bool {
        matches!(self, Self::Active | Self::Stale)
    }
}

const fn value_notification_eligible(field_class: FieldClass) -> bool {
    matches!(
        field_class,
        FieldClass::KnownText(_) | FieldClass::KnownSafeNonText
    )
}
