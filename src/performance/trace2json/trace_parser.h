// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_TRACE2JSON_TRACE_PARSER_H_
#define SRC_PERFORMANCE_TRACE2JSON_TRACE_PARSER_H_

#include <lib/trace-engine/fields.h>
#include <stdint.h>

#include <array>
#include <ostream>
#include <string>
#include <vector>

#include <trace-reader/reader.h>

#include "src/performance/lib/trace_converters/chromium_exporter.h"

namespace tracing {

class FuchsiaTraceParser {
 public:
  explicit FuchsiaTraceParser(const std::filesystem::path& out);
  ~FuchsiaTraceParser();

  bool ParseComplete(std::istream*);

 private:
  static constexpr size_t kReadBufferSize = trace::RecordFields::kMaxRecordSizeBytes * 4;
  ChromiumExporter exporter_;
  std::array<char, kReadBufferSize> buffer_;
  // The number of bytes of |buffer_| in use.
  size_t buffer_end_ = 0;

  trace::TraceReader reader_;
};

}  // namespace tracing

#endif  // SRC_PERFORMANCE_TRACE2JSON_TRACE_PARSER_H_
