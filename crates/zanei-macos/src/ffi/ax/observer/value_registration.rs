use crate::{
    ffi::ax::{
        NativeAxError,
        cf::{CfRef, OwnedCf},
        element::cf_equal,
    },
    focused_field::FieldClass,
};

const AX_ERROR_NOTIFICATION_ALREADY_REGISTERED: i32 = -25_209;
const AX_ERROR_NOTIFICATION_NOT_REGISTERED: i32 = -25_210;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationState {
    Active,
    Stale,
}

struct Registration {
    element: OwnedCf,
    notification: &'static str,
    state: RegistrationState,
}

#[derive(Default)]
pub(in crate::ffi::ax) struct NotificationRegistry {
    registrations: Vec<Registration>,
}

pub(in crate::ffi::ax) enum RegistrationError {
    Register(NativeAxError),
    Unregister(NativeAxError),
}

impl NotificationRegistry {
    pub(in crate::ffi::ax) fn reconcile(
        &mut self,
        element: &OwnedCf,
        notification: &'static str,
        field_class: FieldClass,
        register: impl FnOnce() -> Result<(), NativeAxError>,
        unregister: impl FnOnce() -> Result<(), NativeAxError>,
    ) -> Result<(), RegistrationError> {
        if value_notification_eligible(field_class) {
            self.register(element, notification, register)
        } else {
            self.unregister(element.as_ptr(), notification, unregister)
        }
    }

    pub(in crate::ffi::ax) fn unregister(
        &mut self,
        element: CfRef,
        notification: &'static str,
        unregister: impl FnOnce() -> Result<(), NativeAxError>,
    ) -> Result<(), RegistrationError> {
        let Some(index) = self.registration_index(element, notification) else {
            return Ok(());
        };
        match unregister() {
            Ok(()) => {
                self.registrations.remove(index);
                Ok(())
            }
            Err(error) if error.code() == AX_ERROR_NOTIFICATION_NOT_REGISTERED => {
                self.registrations.remove(index);
                Ok(())
            }
            Err(error) => {
                self.registrations[index].state = RegistrationState::Stale;
                Err(RegistrationError::Unregister(error))
            }
        }
    }

    pub(in crate::ffi::ax) fn accepts_delivery(
        &self,
        element: CfRef,
        notification: &'static str,
    ) -> bool {
        self.registration_index(element, notification)
            .is_some_and(|index| self.registrations[index].state == RegistrationState::Active)
    }

    pub(in crate::ffi::ax) fn needs_cleanup(
        &self,
        element: CfRef,
        notification: &'static str,
    ) -> bool {
        self.registration_index(element, notification).is_some()
    }

    pub(in crate::ffi::ax) fn registered_notifications(
        &self,
    ) -> impl Iterator<Item = (CfRef, &'static str)> + '_ {
        self.registrations
            .iter()
            .map(|registration| (registration.element.as_ptr(), registration.notification))
    }

    pub(in crate::ffi::ax) fn register(
        &mut self,
        element: &OwnedCf,
        notification: &'static str,
        register: impl FnOnce() -> Result<(), NativeAxError>,
    ) -> Result<(), RegistrationError> {
        let existing = self.registration_index(element.as_ptr(), notification);
        if existing
            .is_some_and(|index| self.registrations[index].state == RegistrationState::Active)
        {
            return Ok(());
        }
        match register() {
            Ok(()) => self.activate(existing, element, notification),
            Err(error) if error.code() == AX_ERROR_NOTIFICATION_ALREADY_REGISTERED => {
                self.activate(existing, element, notification)
            }
            Err(error) => return Err(RegistrationError::Register(error)),
        }
        Ok(())
    }

    fn activate(&mut self, existing: Option<usize>, element: &OwnedCf, notification: &'static str) {
        if let Some(index) = existing {
            self.registrations[index].state = RegistrationState::Active;
            return;
        }
        // The focused target proves this AX element is non-null and live here.
        let element = unsafe { OwnedCf::retain(element.as_ptr()) }
            .expect("focused AX element must be retainable");
        self.registrations.push(Registration {
            element,
            notification,
            state: RegistrationState::Active,
        });
    }

    fn registration_index(&self, element: CfRef, notification: &'static str) -> Option<usize> {
        self.registrations.iter().position(|registration| {
            registration.notification == notification
                && cf_equal(registration.element.as_ptr(), element)
        })
    }
}

const fn value_notification_eligible(field_class: FieldClass) -> bool {
    matches!(
        field_class,
        FieldClass::KnownText(_) | FieldClass::KnownSafeNonText
    )
}
