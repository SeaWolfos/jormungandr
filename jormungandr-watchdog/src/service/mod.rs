mod control;
mod intercom;
mod settings;
mod state;
mod status;

pub use self::{
    control::{Control, ControlReader, Controller},
    intercom::{Intercom, IntercomMsg, IntercomReceiver, IntercomSender, NoIntercom},
    settings::{NoSettings, Settings, SettingsReader, SettingsUpdater},
    state::{NoState, State, StateHandler, StateSaver},
    status::{Status, StatusReader, StatusUpdater},
};
use crate::watchdog::WatchdogQuery;
use async_trait::async_trait;
use futures_util::{
    future::abortable,
    future::{select, Either},
};
use thiserror::Error;
use tokio::runtime::{Builder, Handle, Runtime};

pub type ServiceIdentifier = &'static str;

#[async_trait]
pub trait Service: Send + Sized + 'static {
    const SERVICE_IDENTIFIER: ServiceIdentifier;

    type State: State;
    type Settings: Settings;
    type Intercom: IntercomMsg;

    fn prepare(service_state: ServiceState<Self>) -> Self;

    async fn start(self);
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ServiceError {
    #[error("Service cannot be started because status is: {status}")]
    CannotStart { status: Status },
}

pub struct ServiceManager<T: Service> {
    identifier: ServiceIdentifier,

    settings: SettingsUpdater<T::Settings>,
    state: StateSaver<T::State>,
    intercom_sender: IntercomSender<T::Intercom>,

    status: StatusReader,
    controller: Controller,
    runtime: Runtime,
}

/// not to mistake for `tokio`'s runtime. This is the object that
/// will hold the service process and all the other associated data.
/// to allow for a good running activity of the service.
///
pub struct ServiceRuntime<T: Service> {
    service_state: ServiceState<T>,

    status: StatusUpdater,
    control: ControlReader,
}

pub struct ServiceState<T: Service> {
    pub identifier: ServiceIdentifier,

    pub handle: Handle,
    pub settings: SettingsReader<T::Settings>,
    pub state: StateHandler<T::State>,
    pub intercom_receiver: IntercomReceiver<T::Intercom>,
    pub watchdog_query: WatchdogQuery,
}

impl<T: Service> ServiceManager<T> {
    pub async fn new() -> Self {
        let identifier = T::SERVICE_IDENTIFIER;

        let settings = SettingsUpdater::new(T::Settings::default()).await;
        let state = StateSaver::new(T::State::default()).await;
        let status = StatusReader::new(Status::Shutdown);
        let controller = Controller::new().await;
        let (intercom_sender, _) = intercom::channel();

        let runtime = Builder::new()
            .enable_io()
            .enable_time()
            .thread_name(identifier)
            .threaded_scheduler()
            .build()
            .unwrap();

        Self {
            identifier,
            settings,
            state,
            intercom_sender,
            status,
            controller,
            runtime,
        }
    }

    pub fn intercom(&self) -> IntercomSender<T::Intercom> {
        self.intercom_sender.clone()
    }

    pub fn shutdown(&mut self) {
        match self.status.status() {
            Status::Shutdown | Status::ShuttingDown => {
                // Ignore as the node is either shutdown or already shutting
                // down
            }
            Status::Starting | Status::Started => {
                // send only if the node will have a chance to actually read
                // the command
                self.controller.send(Control::Shutdown)
            }
        }
    }

    pub fn runtime(
        &mut self,
        watchdog_query: WatchdogQuery,
    ) -> Result<ServiceRuntime<T>, ServiceError> {
        let status = self.status.status();
        if status != Status::Shutdown {
            Err(ServiceError::CannotStart { status })
        } else {
            let (intercom_sender, intercom_receiver) = intercom::channel::<T::Intercom>();

            std::mem::replace(&mut self.intercom_sender, intercom_sender);

            Ok(ServiceRuntime {
                service_state: ServiceState {
                    identifier: self.identifier,
                    handle: self.runtime.handle().clone(),
                    settings: self.settings.reader(),
                    state: self.state.handler(),
                    intercom_receiver,
                    watchdog_query,
                },
                status: self.status.updater(),
                control: self.controller.reader(),
            })
        }
    }
}

impl<T: Service> ServiceRuntime<T> {
    pub fn start(self) {
        let ServiceRuntime {
            service_state,
            status,
            mut control,
        } = self;

        status.update(Status::Starting);

        let handle = service_state.handle.clone();
        let runner = T::prepare(service_state);

        let (runner, abort_handle) = abortable(async move { runner.start().await });

        let mut service_join_handle = handle.spawn(runner);

        // the runner (the service) has been started into its current runtime. They must use
        // the `handle` to spawn new tasks.
        //
        // however the control of the service is still spawned in the watchdog current context
        // so we can perform the management tasks without disrupting the service's runtime
        tokio::spawn(async move {
            status.update(Status::Started);

            loop {
                let sjh = std::pin::Pin::new(&mut service_join_handle);
                let control = std::pin::Pin::new(&mut control);
                let control = select(sjh, control).await;

                match control {
                    Either::Right((Some(Control::Shutdown), _)) => {
                        status.update(Status::ShuttingDown);
                        // TODO: send the shutdown signal to the task
                    }
                    Either::Left((Err(join_error), _)) => {
                        // TODO: the task could not join, either cancelled
                        //       or panicked. Ideally we need to document
                        //       this panic and see what kind of strategy
                        //       can be applied (can we restart the service?)
                        //       or is it a fatal panic and we cannot recover?

                        eprintln!(
                            "{}'s main process failed with following error {:#?}",
                            T::SERVICE_IDENTIFIER,
                            join_error
                        );

                        status.update(Status::Shutdown);
                        break;
                    }
                    // If the service join handle has been notified that the
                    // associated task has finished or has been aborted
                    Either::Left((Ok(_), _))
                    // or if the controller received the signal the service's
                    // Controller has been closed
                    | Either::Right((None, _))
                    // or if the object has been signaled to be terminated now
                    | Either::Right((Some(Control::Kill), _)) => {
                        status.update(Status::Shutdown);
                        abort_handle.abort();
                        break;
                    }
                }
            }
        });
    }
}

impl<T: Service> Drop for ServiceManager<T> {
    fn drop(&mut self) {
        if self.status.status() != Status::Shutdown {
            self.controller.send(Control::Kill)
        }
    }
}