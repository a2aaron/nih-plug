use atomic_refcell::AtomicRefMut;
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::Arc;

use super::wrapper::{OutputParamEvent, Task, Wrapper};
use crate::context::gui::GuiContext;
use crate::context::init::InitContext;
use crate::context::process::{ProcessContext, Transport};
use crate::context::PluginApi;
use crate::event_loop::EventLoop;
use crate::midi::PluginNoteEvent;
use crate::params::internals::ParamPtr;
use crate::plugin::ClapPlugin;

/// An [`InitContext`] implementation for the wrapper.
///
/// # Note
///
/// See the VST3 `WrapperInitContext` for an explanation of why we need this `pending_requests`
/// field.
pub(crate) struct WrapperInitContext<'a, P: ClapPlugin> {
    pub(super) wrapper: &'a Wrapper<P>,
    pub(super) pending_requests: PendingInitContextRequests,
}

/// Any requests that should be sent out when the [`WrapperInitContext`] is dropped. See that
/// struct's docstring for mroe information.
#[derive(Debug, Default)]
pub(crate) struct PendingInitContextRequests {
    /// The value of the last `.set_latency_samples()` call.
    latency_changed: Cell<Option<u32>>,
}

/// A [`ProcessContext`] implementation for the wrapper. This is a separate object so it can hold on
/// to lock guards for event queues. Otherwise reading these events would require constant
/// unnecessary atomic operations to lock the uncontested RwLocks.
pub(crate) struct WrapperProcessContext<'a, P: ClapPlugin> {
    pub(super) wrapper: &'a Wrapper<P>,
    pub(super) input_events_guard: AtomicRefMut<'a, VecDeque<PluginNoteEvent<P>>>,
    pub(super) output_events_guard: AtomicRefMut<'a, VecDeque<PluginNoteEvent<P>>>,
    pub(super) transport: Transport,
}

/// A [`GuiContext`] implementation for the wrapper. This is passed to the plugin in
/// [`Editor::spawn()`][crate::prelude::Editor::spawn()] so it can interact with the rest of the plugin and
/// with the host for things like setting parameters.
pub(crate) struct WrapperGuiContext<P: ClapPlugin> {
    pub(super) wrapper: Arc<Wrapper<P>>,
    #[cfg(debug_assertions)]
    pub(super) param_gesture_checker:
        atomic_refcell::AtomicRefCell<crate::wrapper::util::context_checks::ParamGestureChecker>,
}

impl<P: ClapPlugin> Drop for WrapperInitContext<'_, P> {
    fn drop(&mut self) {
        if let Some(samples) = self.pending_requests.latency_changed.take() {
            self.wrapper.set_latency_samples(samples)
        }
    }
}

impl<P: ClapPlugin> InitContext<P> for WrapperInitContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }

    fn execute(&self, task: P::BackgroundTask) {
        (self.wrapper.task_executor.lock())(task);
    }

    fn set_latency_samples(&self, samples: u32) {
        // See this struct's docstring
        self.pending_requests.latency_changed.set(Some(samples));
    }

    fn set_current_voice_capacity(&self, capacity: u32) {
        self.wrapper.set_current_voice_capacity(capacity)
    }
}

impl<P: ClapPlugin> ProcessContext<P> for WrapperProcessContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }

    fn execute_background(&self, task: P::BackgroundTask) {
        let task_posted = self.wrapper.schedule_background(Task::PluginTask(task));
        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    fn execute_gui(&self, task: P::BackgroundTask) {
        let task_posted = self.wrapper.schedule_gui(Task::PluginTask(task));
        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    #[inline]
    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        self.input_events_guard.pop_front()
    }

    fn peek_event(&self) -> Option<&PluginNoteEvent<P>> {
        self.input_events_guard.front()
    }

    fn send_event(&mut self, event: PluginNoteEvent<P>) {
        self.output_events_guard.push_back(event);
    }

    fn set_latency_samples(&self, samples: u32) {
        self.wrapper.set_latency_samples(samples)
    }

    fn set_current_voice_capacity(&self, capacity: u32) {
        self.wrapper.set_current_voice_capacity(capacity)
    }
}

impl<P: ClapPlugin> GuiContext for WrapperGuiContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }

    fn request_resize(&self) -> bool {
        self.wrapper.request_resize()
    }

    // All of these functions are supposed to be called from the main thread, so we'll put some
    // trust in the caller and assume that this is indeed the case
    unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
        match self.wrapper.param_ptr_to_hash.get(&param) {
            Some(hash) => {
                let success = self
                    .wrapper
                    .queue_parameter_event(OutputParamEvent::BeginGesture { param_hash: *hash });

                nih_debug_assert!(
                    success,
                    "Parameter output event queue was full, parameter change will not be sent to \
                     the host"
                );
            }
            None => nih_debug_assert_failure!("Unknown parameter: {:?}", param),
        }

        #[cfg(debug_assertions)]
        match self.wrapper.param_id_from_ptr(param) {
            Some(param_id) => self
                .param_gesture_checker
                .borrow_mut()
                .begin_set_parameter(param_id),
            None => nih_debug_assert_failure!(
                "raw_begin_set_parameter() called with an unknown ParamPtr"
            ),
        }
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        match self.wrapper.param_ptr_to_hash.get(&param) {
            Some(hash) => {
                // We queue the parameter change event here, and it will be sent to the host either
                // at the end of the current processing cycle or after requesting an explicit flush
                // (when the plugin isn't processing audio). The parameter's actual value will only
                // be changed when the output event is written to prevent changing parameter values
                // in the middle of processing audio.
                let clap_plain_value = normalized as f64 * param.step_count().unwrap_or(1) as f64;
                let success = self
                    .wrapper
                    .queue_parameter_event(OutputParamEvent::SetValue {
                        param_hash: *hash,
                        clap_plain_value,
                    });

                nih_debug_assert!(
                    success,
                    "Parameter output event queue was full, parameter change will not be sent to \
                     the host"
                );
            }
            None => nih_debug_assert_failure!("Unknown parameter: {:?}", param),
        }

        #[cfg(debug_assertions)]
        match self.wrapper.param_id_from_ptr(param) {
            Some(param_id) => self
                .param_gesture_checker
                .borrow_mut()
                .set_parameter(param_id),
            None => {
                nih_debug_assert_failure!("raw_set_parameter() called with an unknown ParamPtr")
            }
        }
    }

    unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
        match self.wrapper.param_ptr_to_hash.get(&param) {
            Some(hash) => {
                let success = self
                    .wrapper
                    .queue_parameter_event(OutputParamEvent::EndGesture { param_hash: *hash });

                nih_debug_assert!(
                    success,
                    "Parameter output event queue was full, parameter change will not be sent to \
                     the host"
                );
            }
            None => nih_debug_assert_failure!("Unknown parameter: {:?}", param),
        }

        #[cfg(debug_assertions)]
        match self.wrapper.param_id_from_ptr(param) {
            Some(param_id) => self
                .param_gesture_checker
                .borrow_mut()
                .end_set_parameter(param_id),
            None => {
                nih_debug_assert_failure!("raw_end_set_parameter() called with an unknown ParamPtr")
            }
        }
    }

    fn get_state(&self) -> crate::wrapper::state::PluginState {
        self.wrapper.get_state_object()
    }

    fn set_state(&self, state: crate::wrapper::state::PluginState) {
        self.wrapper.set_state_object_from_gui(state)
    }
}
