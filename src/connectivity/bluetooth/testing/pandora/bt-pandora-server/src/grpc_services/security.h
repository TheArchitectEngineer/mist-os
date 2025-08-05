// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_TESTING_PANDORA_BT_PANDORA_SERVER_SRC_GRPC_SERVICES_SECURITY_H_
#define SRC_CONNECTIVITY_BLUETOOTH_TESTING_PANDORA_BT_PANDORA_SERVER_SRC_GRPC_SERVICES_SECURITY_H_

#include <fidl/fuchsia.bluetooth.sys/cpp/fidl.h>

#include "third_party/github.com/google/bt-test-interfaces/src/pandora/security.grpc.pb.h"

class SecurityStorageService : public pandora::SecurityStorage::Service {
  ::grpc::Status IsBonded(::grpc::ServerContext* context, const ::pandora::IsBondedRequest* request,
                          ::google::protobuf::BoolValue* response) override;

  ::grpc::Status DeleteBond(::grpc::ServerContext* context,
                            const ::pandora::DeleteBondRequest* request,
                            ::google::protobuf::Empty* response) override;
};

class SecurityService : public pandora::Security::Service {
 public:
  explicit SecurityService(async_dispatcher_t* dispatcher);

  ::grpc::Status OnPairing(
      ::grpc::ServerContext* context,
      ::grpc::ServerReaderWriter<::pandora::PairingEvent, ::pandora::PairingEventAnswer>* stream)
      override;

  ::grpc::Status Secure(::grpc::ServerContext* context, const ::pandora::SecureRequest* request,
                        ::pandora::SecureResponse* response) override;

  ::grpc::Status WaitSecurity(::grpc::ServerContext* context,
                              const ::pandora::WaitSecurityRequest* request,
                              ::pandora::WaitSecurityResponse* response) override;

 private:
  class PairingDelegateImpl : public fidl::Server<fuchsia_bluetooth_sys::PairingDelegate> {
   public:
    explicit PairingDelegateImpl(
        std::mutex& m_pairing_stream,
        ::grpc::ServerReaderWriter<::pandora::PairingEvent, ::pandora::PairingEventAnswer>**
            pairing_stream)
        : m_pairing_stream_(m_pairing_stream), pairing_stream_(pairing_stream) {}
    void OnPairingRequest(OnPairingRequestRequest& request,
                          OnPairingRequestCompleter::Sync& completer) override;
    void OnPairingComplete(OnPairingCompleteRequest& request,
                           OnPairingCompleteCompleter::Sync& completer) override;
    void OnRemoteKeypress(OnRemoteKeypressRequest& request,
                          OnRemoteKeypressCompleter::Sync& completer) override {}

   private:
    // These point to the corresponding fields in the containing object.
    std::mutex& m_pairing_stream_;
    ::grpc::ServerReaderWriter<::pandora::PairingEvent, ::pandora::PairingEventAnswer>**
        pairing_stream_;
  };

  fidl::SyncClient<fuchsia_bluetooth_sys::Pairing> pairing_client_;

  std::mutex m_pairing_stream_;
  // If this stream is non-null, it means a Security.OnPairing bidirectional stream is active.
  ::grpc::ServerReaderWriter<::pandora::PairingEvent, ::pandora::PairingEventAnswer>*
      pairing_stream_ = nullptr;
};

#endif  // SRC_CONNECTIVITY_BLUETOOTH_TESTING_PANDORA_BT_PANDORA_SERVER_SRC_GRPC_SERVICES_SECURITY_H_
