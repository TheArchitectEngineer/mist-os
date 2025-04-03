#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for memory.py."""

import unittest

from trace_processing import trace_model
from trace_processing.metrics import memory


class MemoryTest(unittest.TestCase):
    @staticmethod
    def construct_trace_model(event_count: int) -> trace_model.Model:
        events: list[trace_model.Event] = [
            trace_model.CounterEvent.from_dict(
                {
                    "cat": "memory:kernel",
                    "name": "kmem_stats_a",
                    "pid": 0x8C01_1EC7_EDDA_7A10,
                    "tid": 0x8C01_1EC7_EDDA_7A20,
                    "ts": 500000 + i,  # microseconds
                    "args": {
                        "total_memory": 1000 + i,
                        "free_bytes": 200 + i,
                        "stall_time_some_ns": 10 + i,
                    },
                }
            )
            for i in range(0, event_count)
        ]

        fake_power_process = trace_model.Process(
            0x8C01_1EC7_EDDA_7A10,
            "MemoryData",
            [
                trace_model.Thread(
                    0x8C01_1EC7_EDDA_7A20,
                    "Fake",
                    events,
                ),
            ],
        )

        model = trace_model.Model()
        threads = [trace_model.Thread(1, f"thread-1")]
        model.processes = [
            trace_model.Process(1000, "load_generator.cm", threads),
            fake_power_process,
        ]
        return model

    def test_process_metrics(self) -> None:
        model = self.construct_trace_model(100)
        metrics = memory.MemoryMetricsProcessor().process_metrics(model)
        self.assertEqual(metrics, [])

    def test_process_freeform_metrics(self) -> None:
        processor = memory.MemoryMetricsProcessor()
        name, metrics = processor.process_freeform_metrics(
            self.construct_trace_model(100)
        )
        self.assertEqual(name, processor.FREEFORM_METRICS_FILENAME)
        self.assertEqual(
            metrics,
            {
                "kernel": {
                    "total_memory": {
                        "P5": 1004.95,
                        "P25": 1024.75,
                        "P50": 1049.5,
                        "P75": 1074.25,
                        "P95": 1094.05,
                        "Min": 1000,
                        "Max": 1099,
                        "Average": 1049.5,
                    },
                    "free_bytes": {
                        "P5": 204.95,
                        "P25": 224.75,
                        "P50": 249.5,
                        "P75": 274.25,
                        "P95": 294.05,
                        "Min": 200,
                        "Max": 299,
                        "Average": 249.5,
                    },
                    "stall_time_some_ns": {
                        "Delta": 99,
                        "Rate": 0.001,
                    },
                }
            },
        )

    def test_process_single_sample(self) -> None:
        processor = memory.MemoryMetricsProcessor()
        name, metrics = processor.process_freeform_metrics(
            self.construct_trace_model(1)
        )
        self.assertEqual(name, processor.FREEFORM_METRICS_FILENAME)
        print(metrics)
        self.assertEqual(
            metrics,
            {
                "kernel": {
                    "total_memory": {
                        "P5": 1000.0,
                        "P25": 1000.0,
                        "P50": 1000.0,
                        "P75": 1000.0,
                        "P95": 1000.0,
                        "Min": 1000,
                        "Max": 1000,
                        "Average": 1000,
                    },
                    "free_bytes": {
                        "P5": 200.0,
                        "P25": 200.0,
                        "P50": 200.0,
                        "P75": 200.0,
                        "P95": 200.0,
                        "Min": 200,
                        "Max": 200,
                        "Average": 200,
                    },
                    "stall_time_some_ns": {"Delta": 0, "Rate": None},
                }
            },
        )
