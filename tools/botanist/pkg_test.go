// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package botanist

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"testing"
)

// emptyPackageRepo is a helper that constructs an empty package repository
// with the expected structure.
func emptyPackageRepo(t *testing.T) string {
	t.Helper()
	localRepo := t.TempDir()

	repoPath := filepath.Join(localRepo, "repository")
	if err := os.Mkdir(repoPath, os.ModePerm); err != nil {
		t.Fatalf("Mkdir(%s) failed; got %s, want <nil> error", repoPath, err)
	}

	rootJSONPath := filepath.Join(repoPath, "root.json")
	f, err := os.Create(rootJSONPath)
	if err != nil {
		t.Fatalf("Create(%s) failed; got %s, want <nil> error", rootJSONPath, err)
	}
	defer f.Close()

	blobPath := filepath.Join(localRepo, "blobs")
	if err := os.Mkdir(blobPath, os.ModePerm); err != nil {
		t.Fatalf("Mkdir(%s) failed; got %s, want <nil> error", blobPath, err)
	}
	return localRepo
}

func TestPackageServer(t *testing.T) {
	testCases := []struct {
		name           string
		endpoint       string
		local          bool
		wantStatusCode int
		wantContents   string
	}{
		{
			name:           "local repository fetches work as expected",
			endpoint:       "/repository/targets.json",
			local:          true,
			wantStatusCode: http.StatusOK,
			wantContents:   "{\"targets\": []}",
		},
		{
			name:           "local blob fetches work as expected",
			endpoint:       "/blobs/123456",
			local:          true,
			wantStatusCode: http.StatusOK,
			wantContents:   "desired contents",
		},
	}
	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			// Set up the local package repository.
			localRepo := emptyPackageRepo(t)

			// Add any contents to the repo that the test case requires.
			if tc.wantContents != "" {
				localPath := filepath.Join(localRepo, tc.endpoint)
				if err := os.WriteFile(localPath, []byte(tc.wantContents), os.ModePerm); err != nil {
					t.Fatalf("WriteFile(%s, %s) failed; got %s, want <nil> error", localPath, tc.wantContents, err)
				}
			}

			// Start the package server.
			// We ignore the returned repoURL and blobURL to make testing
			// invalid endpoints easier.
			pkgSrv, err := NewPackageServer(ctx, localRepo, 8080)
			if err != nil {
				t.Fatalf("NewPackageServer failed; got %s, want <nil> error", err)
			}
			defer pkgSrv.Close()

			// Make a request to the endpoint and validate that we get the
			// expected response.
			addr := fmt.Sprintf("http://%s:8080%s", localhostPlaceholder, tc.endpoint)
			res, err := http.Get(addr)
			if err != nil {
				t.Fatalf("http.Get(%s) failed; got %s, want <nil> error", addr, err)
			}
			defer res.Body.Close()

			if res.StatusCode != tc.wantStatusCode {
				t.Errorf("got incorrect status code; got %d, want %d", res.StatusCode, tc.wantStatusCode)
			}
			if tc.wantContents != "" {
				body, err := io.ReadAll(res.Body)
				if err != nil {
					t.Fatalf("reading response body failed; got %s, want <nil> error", err)
				}
				if tc.wantContents != string(body) {
					t.Errorf("got incorrect contents; got %q, want %q", string(body), tc.wantContents)
				}
			}
		})
	}
}
