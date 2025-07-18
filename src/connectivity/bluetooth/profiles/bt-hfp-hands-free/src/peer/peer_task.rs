// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Result};
use bt_hfp::call::indicators as call_indicators;
use bt_hfp::codec_id::CodecId;
use bt_hfp::{a2dp, audio, sco};
use fuchsia_bluetooth::types::{Channel, PeerId};
use fuchsia_sync::Mutex;
use futures::{select, StreamExt};
use log::{debug, error, info, warn};
use std::sync::Arc;
use vigil::{DropWatch, Vigil};
use {at_commands as at, fidl_fuchsia_bluetooth_hfp as fidl_hfp, fuchsia_async as fasync};

use crate::config::HandsFreeFeatureSupport;
use crate::peer::ag_indicators::{
    AgIndicator, AgIndicatorTranslator, BatteryChargeIndicator, CallIndicator,
    NetworkInformationIndicator,
};
use crate::peer::at_connection::{self, Response as AtResponse};
use crate::peer::calls::{CallOutput, Calls};
use crate::peer::indicated_state::IndicatedState;
use crate::peer::procedure::{CommandFromHf, CommandToHf, ProcedureInput, ProcedureOutput};
use crate::peer::procedure_manager::ProcedureManager;

const DEFAULT_CODEC: CodecId = CodecId::CVSD;

pub struct PeerTask {
    peer_id: PeerId,
    procedure_manager: ProcedureManager<ProcedureInput, ProcedureOutput>,
    peer_handler_request_stream: fidl_hfp::PeerHandlerRequestStream,
    at_connection: at_connection::Connection,
    ag_indicator_translator: AgIndicatorTranslator,
    indicated_state: IndicatedState,
    calls: Calls,
    sco_connector: sco::Connector,
    sco_state: sco::InspectableState,
    a2dp_control: a2dp::Control,
    audio_control: Arc<Mutex<Box<dyn audio::Control>>>,
}

impl PeerTask {
    pub fn spawn(
        peer_id: PeerId,
        hf_features: HandsFreeFeatureSupport,
        peer_handler_request_stream: fidl_hfp::PeerHandlerRequestStream,
        rfcomm: Channel,
        sco_connector: sco::Connector,
        audio_control: Arc<Mutex<Box<dyn audio::Control>>>,
    ) -> fasync::Task<PeerId> {
        let procedure_manager = ProcedureManager::new(peer_id, hf_features);
        let at_connection = at_connection::Connection::new(peer_id, rfcomm);
        let calls = Calls::new(peer_id);
        let ag_indicator_translator = AgIndicatorTranslator::new();
        let indicated_state = IndicatedState::default();
        let sco_state = sco::InspectableState::default();
        let a2dp_control = a2dp::Control::connect();

        let mut peer_task = Self {
            peer_id,
            procedure_manager,
            peer_handler_request_stream,
            at_connection,
            ag_indicator_translator,
            indicated_state,
            calls,
            sco_connector,
            sco_state,
            a2dp_control,
            audio_control,
        };

        // Set SCO state to AwaitingRemote
        peer_task.await_remote_sco();

        let fasync_task = fasync::Task::local(peer_task.run());
        fasync_task
    }

    pub async fn run(mut self) -> PeerId {
        info!(peer:% = self.peer_id; "Starting task.");

        let result = (&mut self).run_inner().await;
        match result {
            Ok(_) => info!(peer:% =self.peer_id; "Successfully finished task."),
            Err(err) => warn!(peer:% = self.peer_id, error:% = err; "Finished task with error"),
        }

        self.peer_id
    }

    async fn run_inner(&mut self) -> Result<()> {
        self.enqueue_command_from_hf(CommandFromHf::StartSlci {
            hf_features: self.procedure_manager.procedure_manipulated_state.hf_features,
        });

        loop {
            // Since IOwned doesn't implement DerefMut, we need to get a mutable reference to the
            // underlying SCO state value to call mutable methods on it.  As this invalidates
            // the enclosing struct, we need to drop this reference explicitiy in every match arm so
            // we can call method on self.
            let mut sco_state = self.sco_state.as_mut();
            let mut on_sco_closed = sco_state.on_closed();

            select! {
                peer_handler_request_result_option = self.peer_handler_request_stream.next() => {
                    debug!("Received FIDL PeerHandler protocol request {:?} from peer {}",
                       peer_handler_request_result_option, self.peer_id);

                    drop(sco_state);

                    let peer_handler_request_result = peer_handler_request_result_option
                        .ok_or_else(|| format_err!("FIDL Peer protocol request stream closed for peer {}", self.peer_id))?;
                    let peer_handler_request = peer_handler_request_result?;

                    self.handle_peer_handler_request(peer_handler_request);
                }
                at_response_result_option = self.at_connection.next() => {
                // TODO(https://fxbug.dev/127362) Filter unsolicited AT messages.
                    debug!("Received AT response {:?} from peer {}",
                        at_response_result_option, self.peer_id);

                    drop(sco_state);

                    let at_response_result =
                        at_response_result_option
                            .ok_or_else(|| format_err!("AT connection stream closed for peer {}", self.peer_id))?;
                    let at_response = at_response_result?;

                    self.handle_at_response(at_response);
                }
                procedure_outputs_result_option = self.procedure_manager.next() => {
                    debug!("Received procedure outputs {:?} for peer {:?}",
                        procedure_outputs_result_option, self.peer_id);

                    drop(sco_state);

                    let procedure_outputs_result =
                        procedure_outputs_result_option
                            .ok_or_else(|| format_err!("Procedure manager stream closed for peer {}", self.peer_id))?;

                    let procedure_outputs = match procedure_outputs_result {
                        Ok(procedure_outputs) => procedure_outputs,
                        Err(err) => {
                            // TODO(b/327283873) Remove this when all AT commands are handled.
                            warn!("Procedure error: {:?}", err);
                            continue;
                        }
                    };

                    for procedure_output in procedure_outputs {
                        self.handle_procedure_output(procedure_output).await?;
                    }
                }
                call_output_option = self.calls.next() => {
                    info!("Received call output {:?} for peer {:}", call_output_option, self.peer_id);

                    drop(sco_state);

                    let call_output =
                        call_output_option
                            .ok_or_else(|| format_err!("Calls stream closed for peer {:}", self.peer_id))?;
                    self.handle_call_output(call_output);
                }
                sco_connection_result = sco_state.on_connected() => {
                    info!("Received SCO connection for peer {:}", self.peer_id);
                    drop(sco_state);
                    match sco_connection_result {
                        Ok(sco_connection) if !sco_connection.is_closed() => self.handle_sco_connection(sco_connection).await?,
                        // This can occur if the AG opens and closes a SCO connection immediately.
                        Ok(_closed_sco_connection) => info!("Got closed SCO connection from peer {}", self.peer_id),
                        Err(err) => {
                            warn!("Got error receiving SCO connection for peer {}.", self.peer_id);
                            Err(err)?;
                        }
                    }
                }
                _ = on_sco_closed => {
                    info!("SCO connection closed for peer {:}", self.peer_id);
                    drop(sco_state);
                    self.await_remote_sco();
                }
            }
        }
    }

    fn handle_peer_handler_request(&mut self, peer_handler_request: fidl_hfp::PeerHandlerRequest) {
        debug!("Got peer handler request {:?} from peer {:}", peer_handler_request, self.peer_id);

        match peer_handler_request {
            fidl_hfp::PeerHandlerRequest::RequestOutgoingCall {
                action: fidl_hfp::CallAction::DialFromNumber(number),
                responder,
            } => {
                self.log_responder_error(responder.send(Ok(())));
                self.enqueue_command_from_hf(CommandFromHf::CallActionDialFromNumber { number });
            }
            fidl_hfp::PeerHandlerRequest::RequestOutgoingCall {
                action: fidl_hfp::CallAction::DialFromLocation(memory),
                responder,
            } => {
                self.log_responder_error(responder.send(Ok(())));
                self.enqueue_command_from_hf(CommandFromHf::CallActionDialFromMemory { memory });
            }
            fidl_hfp::PeerHandlerRequest::RequestOutgoingCall {
                action: fidl_hfp::CallAction::RedialLast(_),
                responder,
            } => {
                self.log_responder_error(responder.send(Ok(())));
                self.enqueue_command_from_hf(CommandFromHf::CallActionRedialLast);
            }
            fidl_hfp::PeerHandlerRequest::RequestOutgoingCall {
                action: fidl_hfp::CallAction::TransferActive(_),
                responder,
            } => {
                self.start_audio_connection();
                self.log_responder_error(responder.send(Ok(())));
            }
            fidl_hfp::PeerHandlerRequest::WatchNextCall { responder } => {
                self.calls.handle_watch_next_call(responder);
            }
            fidl_hfp::PeerHandlerRequest::WatchNetworkInformation { responder } => {
                self.indicated_state.handle_watch_network_information(responder);
            }
            other => {
                error!(
                    "Unimplemented PeerHandler FIDL request {:?} for peer {:}",
                    other, self.peer_id
                );
            }
        };
    }

    fn log_responder_error(&self, send_result: Result<(), fidl::Error>) {
        if let Err(err) = send_result {
            warn!("Error {:?} sending result to peer {:}", err, self.peer_id);
        }
    }

    fn enqueue_command_from_hf(&mut self, command_from_hf: CommandFromHf) {
        self.procedure_manager.enqueue(ProcedureInput::CommandFromHf(command_from_hf));
    }

    fn handle_at_response(&mut self, at_response: AtResponse) {
        if Self::at_response_is_unsolicted(&at_response) {
            self.handle_unsolicited_response(at_response)
        } else {
            let procedure_input = ProcedureInput::AtResponseFromAg(at_response);

            self.procedure_manager.enqueue(procedure_input);
        }
    }

    fn at_response_is_unsolicted(at_response: &AtResponse) -> bool {
        match at_response {
            AtResponse::Recognized(at::Response::Success(at::Success::Ciev { .. }))
            | AtResponse::Recognized(at::Response::Success(at::Success::Clip { .. })) => true,
            _ => false,
        }
    }

    // Must take an unolicited AT Response--a +CIEV or a +CLIP.
    fn handle_unsolicited_response(&mut self, at_response: AtResponse) {
        match at_response {
            AtResponse::Recognized(at::Response::Success(at::Success::Ciev { ind, value })) => {
                let indicator = self.ag_indicator_translator.translate_indicator(ind, value);
                match indicator {
                    Ok(ind) => self.handle_ag_indicator(ind),
                    // HFP v1.8 sec. 4.10: Unknown indicators are not an error.
                    Err(err) => info!("Got unknown indicator: {:?}", err),
                }
            }
            // TODO(htps://fxbug.dec/135158) Handle setting phone numbers.
            AtResponse::Recognized(at::Response::Success(at::Success::Clip { .. })) => {
                warn!("+CLIP not handled")
            }
            _ => warn!("Received unexpected unsolicited AT response: {:?}", at_response),
        }
    }

    fn handle_ag_indicator(&mut self, indicator: AgIndicator) -> () {
        match indicator {
            AgIndicator::Call(call_indicator) => {
                self.calls.set_call_state_by_indicator(call_indicator)
            }
            AgIndicator::NetworkInformation(network_indicator) => {
                self.handle_network_information_indicator(network_indicator)
            }
            AgIndicator::BatteryCharge(BatteryChargeIndicator { percent }) => {
                self.indicated_state.set_ag_battery_level(percent)
            }
        }
    }

    fn handle_network_information_indicator(&mut self, indicator: NetworkInformationIndicator) {
        match indicator {
            NetworkInformationIndicator::ServiceAvailable(service) => {
                self.indicated_state.set_service_available(service)
            }
            NetworkInformationIndicator::SignalStrength(signal) => {
                self.indicated_state.set_signal_strength(signal)
            }
            NetworkInformationIndicator::Roaming(roaming) => {
                self.indicated_state.set_roaming(roaming)
            }
        }
    }

    async fn handle_procedure_output(&mut self, procedure_output: ProcedureOutput) -> Result<()> {
        match procedure_output {
            ProcedureOutput::AtCommandToAg(command) => {
                self.at_connection.write_commands(&[command]).await?
            }
            ProcedureOutput::CommandToHf(CommandToHf::SetInitialAgIndicatorValues {
                ordered_values,
            }) => self.set_initial_ag_indicator_values(ordered_values)?,
            ProcedureOutput::CommandToHf(CommandToHf::SetAgIndicatorIndex { indicator, index }) => {
                self.ag_indicator_translator.set_index(indicator, index)?
            }
            ProcedureOutput::CommandToHf(CommandToHf::AwaitRemoteSco) => {
                // We've renegotiated the codec, so wait for a SCO connection with the new codec.
                self.await_remote_sco()
            }
        };

        Ok(())
    }

    fn set_initial_ag_indicator_values(&mut self, ordered_values: Vec<i64>) -> Result<()> {
        // Indices are 1-indexed.
        let indices_and_values = std::iter::zip(1i64.., ordered_values.into_iter());

        for (index, value) in indices_and_values {
            let index: i64 = index.try_into().expect("Failed to fit AG indicator index into i64?");
            let ag_indicator = self.ag_indicator_translator.translate_indicator(index, value)?;
            match ag_indicator {
                AgIndicator::Call(CallIndicator::Call(call))
                    if call != call_indicators::Call::None =>
                {
                    Err(format_err!(
                        "Got unexpected initial call indicator value {:?} for peer {:}",
                        call,
                        self.peer_id
                    ))?;
                }
                AgIndicator::Call(CallIndicator::CallSetup(call_setup))
                    if call_setup != call_indicators::CallSetup::None =>
                {
                    Err(format_err!(
                        "Got unexpected initial callsetup indicator value {:?} for peer {:}",
                        call_setup,
                        self.peer_id
                    ))?;
                }
                AgIndicator::Call(CallIndicator::CallHeld(call_held))
                    if call_held != call_indicators::CallHeld::None =>
                {
                    Err(format_err!(
                        "Got unexpected initial callheld indicator value {:?} for peer {:}",
                        call_held,
                        self.peer_id
                    ))?;
                }
                AgIndicator::Call(_) => { // Nothing to do
                }
                AgIndicator::NetworkInformation(network_indicator) => {
                    self.handle_network_information_indicator(network_indicator);
                }
                AgIndicator::BatteryCharge(BatteryChargeIndicator { percent }) => {
                    self.indicated_state.set_ag_battery_level(percent)
                }
            }
        }

        self.indicated_state.initial_indicators_set();

        Ok(())
    }

    fn handle_call_output(&mut self, call_output: CallOutput) {
        match call_output {
            CallOutput::ProcedureInput(call_procedure_input) => {
                self.procedure_manager.enqueue(call_procedure_input)
            }
            CallOutput::TransferCallToAg => {
                self.close_sco();
            }
        }
    }

    async fn handle_sco_connection(&mut self, sco_connection: sco::Connection) -> Result<()> {
        self.calls.set_sco_connected(true);

        let pause_token = self.pause_a2dp_audio().await?;
        let vigil = self.watch_active_sco(&sco_connection, pause_token);
        self.start_hfp_audio(sco_connection)?;

        self.sco_state.iset(sco::State::Active(vigil));

        Ok(())
    }

    async fn pause_a2dp_audio(&mut self) -> Result<a2dp::PauseToken> {
        let res = self.a2dp_control.pause(Some(self.peer_id.clone())).await;
        let pause_token = match res {
            Err(err) => {
                Err(format_err!("Couldn't pause A2DP Audio for peer {}: {:?}", self.peer_id, err))
            }
            Ok(token) => {
                info!("Successfully paused A2DP audio for peer {}", self.peer_id);
                Ok(token)
            }
        };

        pause_token
    }

    fn start_hfp_audio(&self, sco_connection: sco::Connection) -> Result<()> {
        let mut audio = self.audio_control.lock();
        let codec_id = CodecId::from_parameter_set(&sco_connection.params.parameter_set);
        if let Err(err) = audio.start(self.peer_id.clone(), sco_connection, codec_id) {
            Err(format_err!("Couldn't start HFP audio for peer {}: {:?}", self.peer_id, err))?;
        } else {
            info!("Successfully started HFP audio for peer {}", self.peer_id);
        }

        Ok(())
    }

    fn watch_active_sco(
        &self,
        sco_connection: &sco::Connection,
        pause_token: a2dp::PauseToken,
    ) -> Vigil<sco::Active> {
        let vigil = Vigil::new(sco::Active::new(sco_connection, pause_token));
        let peer_id = self.peer_id.clone();
        let audio_control = self.audio_control.clone();

        Vigil::watch(&vigil, {
            move |_| match audio_control.lock().stop(peer_id) {
                Err(err) => warn!("Couldn't stop HFP audio for peer {}: {:?}", peer_id, err),
                Ok(()) => info!("Succesfully stopped HFP audio, for peer {}", peer_id),
            }
        });

        vigil
    }

    fn start_audio_connection(&mut self) {
        self.enqueue_command_from_hf(CommandFromHf::StartAudioConnection);
    }

    fn close_sco(&mut self) {
        // Drop SCO connection before awaiting a new one.
        self.sco_state.iset(sco::State::Inactive);
        self.await_remote_sco();
    }

    fn get_selected_codec(&self) -> CodecId {
        let codec_option = self.procedure_manager.procedure_manipulated_state.selected_codec;
        codec_option.unwrap_or(DEFAULT_CODEC)
    }

    fn await_remote_sco(&mut self) {
        self.calls.set_sco_connected(false);

        let codecs = vec![self.get_selected_codec()];
        let fut = self.sco_connector.accept(self.peer_id.clone(), codecs);
        self.sco_state.iset(sco::State::AwaitingRemote(Box::pin(fut)));
    }
}
