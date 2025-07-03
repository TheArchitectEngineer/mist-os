// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testsharder

import (
	"fmt"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/proto"
	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
)

var x64EmuEnv = build.Environment{
	Dimensions: build.DimensionSet{"device_type": "QEMU", "cpu": "x64"},
}
var arm64EmuEnv = build.Environment{
	Dimensions: build.DimensionSet{"device_type": "QEMU", "cpu": "arm64"},
}
var nucEnv = build.Environment{
	Dimensions: build.DimensionSet{"device_type": "NUC"},
}
var vim3Env = build.Environment{
	Dimensions: build.DimensionSet{"device_type": "Vim3"},
}
var linuxEnv = build.Environment{
	Dimensions: build.DimensionSet{"os": "Linux", "cpu": "x64"},
}
var gceEnv = build.Environment{
	Dimensions: build.DimensionSet{"device_type": "GCE"},
}
var macEnv = build.Environment{
	Dimensions: build.DimensionSet{"os": "Mac", "cpu": "x64"},
}

func TestGetBotDimensions(t *testing.T) {
	testCases := []struct {
		name       string
		env        build.Environment
		os         string
		cpu        string
		expectsSSH bool
		params     *proto.Params
		want       map[string]string
	}{
		{
			name:       "x64 emulator with ssh",
			env:        x64EmuEnv,
			cpu:        "x64",
			expectsSSH: true,
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "os": "Debian", "gce": "1", "cores": "8", "cpu": "x64"},
		},
		{
			name: "x64 emulator without ssh",
			env:  x64EmuEnv,
			cpu:  "x64",
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "os": "Debian", "gce": "1", "cores": "8", "cpu": "x64"},
		},
		{
			name:       "arm64 emulator with ssh",
			env:        arm64EmuEnv,
			cpu:        "arm64",
			expectsSSH: true,
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "os": "Debian", "cpu": "arm64"},
		},
		{
			name: "arm64 emulator without ssh",
			env:  arm64EmuEnv,
			cpu:  "arm64",
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "os": "Debian", "cpu": "arm64"},
		},
		{
			name:       "arm64 emulator tcg",
			env:        arm64EmuEnv,
			cpu:        "arm64",
			expectsSSH: true,
			params: &proto.Params{
				Pool:   "pool",
				UseTcg: true,
			},
			want: map[string]string{"pool": "pool", "os": "Debian", "gce": "1", "cores": "8", "cpu": "x64"},
		},
		{
			name:       "device with ssh",
			env:        nucEnv,
			cpu:        "x64",
			expectsSSH: true,
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "device_type": "NUC"},
		},
		{
			name: "device without ssh",
			env:  nucEnv,
			cpu:  "x64",
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "serial": "1", "device_type": "NUC"},
		},
		{
			name:       "GCE",
			env:        gceEnv,
			cpu:        "x64",
			expectsSSH: true,
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "os": "Linux", "gce": "1", "cores": "2"},
		},
		{
			name: "linux",
			env:  linuxEnv,
			cpu:  "x64",
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "kvm": "1", "os": "Linux", "gce": "1", "cores": "8", "cpu": "x64"},
		},
		{
			name: "mac",
			env:  macEnv,
			cpu:  "x64",
			params: &proto.Params{
				Pool: "pool",
			},
			want: map[string]string{"pool": "pool", "os": "Mac", "cpu": "x64"},
		},
	}
	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			test := makeTest(1, tc.os)
			test.CPU = tc.cpu
			shard := &Shard{
				Name:       environmentName(tc.env),
				Tests:      []Test{test},
				Env:        tc.env,
				ExpectsSSH: tc.expectsSSH,
				UseTCG:     tc.params.UseTcg,
			}
			GetBotDimensions(shard, tc.params)
			if diff := cmp.Diff(tc.want, shard.BotDimensions); diff != "" {
				t.Errorf("Wrong bot dimensions (-want +got):\n%s", diff)
			}
		})
	}
}

func TestConstructTestsJSON(t *testing.T) {
	tests := []Test{
		{
			Test: build.Test{Name: "test1"},
		}, {
			Test: build.Test{Name: "test2"},
		},
	}
	shard := &Shard{Name: "shard1", Tests: tests}
	buildDir := t.TempDir()
	err := ConstructTestsJSON(shard, buildDir)
	if err != nil {
		t.Fatalf("failed to construct tests.json: %s", err)
	}
	expectedTestsJSON := filepath.Join(buildDir, "shard1_tests.json")
	actualTestsJSON := filepath.Join(buildDir, shard.TestsJSON)
	if actualTestsJSON != expectedTestsJSON {
		t.Errorf("unexpected shard.TestsJSON: %s, want %s", actualTestsJSON, expectedTestsJSON)
	}
	var actualTests []Test
	if err := jsonutil.ReadFromFile(actualTestsJSON, &actualTests); err != nil {
		t.Errorf("failed to read tests.json: %s", err)
	}
	if diff := cmp.Diff(tests, actualTests); diff != "" {
		t.Errorf("Wrong tests.json (-want +got):\n%s", diff)
	}
	if diff := cmp.Diff([]string{"shard1_tests.json"}, shard.Deps); diff != "" {
		t.Errorf("Wrong shard.Deps (-want +got):\n%s", diff)
	}
}

func TestGetEnabledExperiments(t *testing.T) {
	experiments := []string{"exp1:0", "exp2:50", "exp3:100", "exp4:not-a-percentage", "exp5:40", "exp6"}
	origRandInt := randInt
	defer func() {
		randInt = origRandInt
	}()
	randInt = func(_ int) int {
		return 49
	}
	actual, err := GetEnabledExperiments(experiments)
	if err != nil {
		t.Fatalf("failed to get enabled experiments: %s", err)
	}
	expected := []string{"exp2", "exp3", "exp4:not-a-percentage", "exp6"}
	if diff := cmp.Diff(expected, actual); diff != "" {
		t.Fatalf("unexpected enabled experiments (-want +got):\n%s", diff)
	}
}

func TestConstructBaseCommand(t *testing.T) {
	toolNames := []string{"botanist", "ffx", "llvm-profdata"}
	tools := build.Tools{}
	for _, name := range toolNames {
		for _, cpu := range []string{"x64", "arm64"} {
			tools = append(tools, build.Tool{
				Name: name,
				OS:   "linux",
				CPU:  cpu,
				Path: fmt.Sprintf("host_%s/%s", cpu, name),
			})
		}
	}
	testCases := []struct {
		name        string
		shard       *Shard
		netboot     bool
		targetCPU   string
		params      *proto.Params
		variants    []string
		experiments []string
		wantCmd     []string
		wantDeps    []string
		wantErr     bool
	}{
		{
			name: "linux shard",
			shard: &Shard{
				Env: linuxEnv,
			},
			targetCPU: "x64",
			wantCmd:   []string{"./host_x64/botanist", "-level", "debug", "run", "-skip-setup"},
			wantDeps:  []string{"host_x64/botanist"},
		},
		{
			name: "mac shard",
			shard: &Shard{
				Env: macEnv,
			},
			targetCPU: "x64",
			wantCmd:   []string{"./host_x64/botanist", "-level", "debug", "run", "-skip-setup"},
			wantDeps:  []string{"host_x64/botanist"},
		},
		{
			name: "emu x64 shard with ssh",
			shard: &Shard{
				Env:           x64EmuEnv,
				ExpectsSSH:    true,
				ProductBundle: "core.x64",
				PkgRepo:       "repo",
				TimeoutSecs:   600,
			},
			targetCPU: "x64",
			wantCmd: []string{"./host_x64/botanist", "-level", "debug", "run", "-images", "images.json", "-timeout", "600s",
				"-ffx", "./host_x64/ffx", "-product-bundles", "product_bundles.json", "-product-bundle-name", "core.x64",
				"-local-repo", "repo", "-expects-ssh"},
			wantDeps: []string{"host_x64/botanist", "host_x64/ffx"},
		},
		{
			name: "emu x64 shard netboot, no ssh",
			shard: &Shard{
				Env:           x64EmuEnv,
				ProductBundle: "core.x64",
			},
			netboot:   true,
			targetCPU: "x64",
			wantCmd: []string{"./host_x64/botanist", "-level", "debug", "run", "-images", "images.json", "-timeout", "0s",
				"-ffx", "./host_x64/ffx", "-product-bundles", "product_bundles.json", "-product-bundle-name",
				"core.x64", "-use-serial", "-netboot"},
			wantDeps: []string{"host_x64/botanist", "host_x64/ffx"},
		},
		{
			name: "emu arm64 shard boot test",
			shard: &Shard{
				Env:               arm64EmuEnv,
				IsBootTest:        true,
				ProductBundle:     "arm64_boot_test",
				BootupTimeoutSecs: 60,
			},
			netboot:   true,
			targetCPU: "arm64",
			wantCmd: []string{"./host_arm64/botanist", "-level", "debug", "run", "-images", "images.json", "-timeout", "0s",
				"-ffx", "./host_arm64/ffx", "-product-bundles", "product_bundles.json", "-product-bundle-name", "arm64_boot_test",
				"-boot-test", "-bootup-timeout", "60s", "-use-serial", "-netboot"},
			wantDeps: []string{"host_arm64/botanist", "host_arm64/ffx"},
		},
		{
			name: "emu arm64 shard with tcg",
			shard: &Shard{
				Env:           arm64EmuEnv,
				ProductBundle: "core.arm64",
				ExpectsSSH:    true,
			},
			params:    &proto.Params{UseTcg: true},
			targetCPU: "arm64",
			wantCmd: []string{"./host_x64/botanist", "-level", "debug", "run", "-images", "images.json", "-timeout", "0s",
				"-ffx", "./host_x64/ffx", "-product-bundles", "product_bundles.json", "-product-bundle-name", "core.arm64",
				"-expects-ssh", "-test-timeout-scale-factor", "2"},
			wantDeps: []string{"host_x64/botanist", "host_x64/ffx"},
		},
		{
			name: "nuc shard",
			shard: &Shard{
				Env:           nucEnv,
				ExpectsSSH:    true,
				ProductBundle: "core.x64",
			},
			targetCPU: "x64",
			variants:  []string{"coverage"},
			wantCmd: []string{"./host_x64/botanist", "-level", "debug", "run", "-llvm-profdata", "host_x64/llvm-profdata=clang",
				"-images", "images.json", "-timeout", "0s", "-ffx", "./host_x64/ffx", "-product-bundles", "product_bundles.json",
				"-product-bundle-name", "core.x64", "-expects-ssh"},
			wantDeps: []string{"host_x64/botanist", "host_x64/ffx", "host_x64/llvm-profdata"},
		},
		{
			name: "vim3 shard",
			shard: &Shard{
				Env:           vim3Env,
				ProductBundle: "core.vim3",
				ExpectsSSH:    true,
			},
			targetCPU:   "arm64",
			params:      &proto.Params{ZirconArgs: []string{"arg1", "arg2"}},
			experiments: []string{"exp1", "exp2"},
			wantCmd: []string{"./host_x64/botanist", "-level", "debug", "run", "-images", "images.json", "-timeout", "0s",
				"-ffx", "./host_x64/ffx", "-experiment", "exp1", "-experiment", "exp2", "-product-bundles", "product_bundles.json",
				"-product-bundle-name", "core.vim3", "-expects-ssh", "-zircon-args", "arg1", "-zircon-args", "arg2"},
			wantDeps: []string{"host_x64/botanist", "host_x64/ffx"},
		},
		{
			name: "missing product bundle",
			shard: &Shard{
				Env: vim3Env,
			},
			targetCPU: "arm64",
			wantErr:   true,
		},
	}
	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			checkoutDir := t.TempDir()
			buildDir := filepath.Join(checkoutDir, "out", "build_dir")
			test := makeTest(1, "")
			test.CPU = tc.targetCPU
			tc.shard.Tests = []Test{test}
			if tc.params == nil {
				tc.params = &proto.Params{}
			}
			tc.shard.UseTCG = tc.params.UseTcg
			tc.shard.Env.Netboot = tc.netboot
			if err := ConstructBaseCommand(tc.shard, checkoutDir, buildDir, tools, tc.params, tc.variants, tc.experiments); err != nil {
				if tc.wantErr {
					return
				}
				t.Fatalf("failed to construct base command: %s", err)
			}
			if tc.shard.RelativeCWD != "out/build_dir" {
				t.Errorf("Wrong relative cwd: %s, want out/build_dir", tc.shard.RelativeCWD)
			}
			if diff := cmp.Diff(tc.wantCmd, tc.shard.BaseCommand); diff != "" {
				t.Errorf("Wrong shard.BaseCommand (-want +got):\n%s", diff)
			}
			if diff := cmp.Diff(tc.wantDeps, tc.shard.Deps); diff != "" {
				t.Errorf("Wrong shard.Deps (-want +got):\n%s", diff)
			}
		})
	}
}
