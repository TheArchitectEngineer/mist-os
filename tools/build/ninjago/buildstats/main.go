// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.package main

// ninja_buildstats is an utility for extracting useful stats out of build
// artifacts from Ninja. It has two modes of operation.
//
// In the first mode, it takes the output of the ninjatrace tool as input:
//
//	$ buildstats --ninjatrace out/default/ninjatrace.json \
//	             --output path/to/output.json
//
// In the second mode, it combines information ninjalog, compdb and ninja graph,
// extracts and serializes build stats from them. This mode is deprecated as it
// is much slower.
//
//	$ buildstats \
//	    --ninjalog out/default/.ninja_log
//	    --compdb path/to/compdb.json
//	    --graph path/to/graph.dot
//	    --output path/to/output.json
//
// Finally, the tool can read and write gzip-compressed files, by specifying
// a .gz prefix in file paths, for example:
//
//	$ buildstats --ninjatrace out/default/ninjatrace.json.gz \
//	             --output path/to/output.json.gz
package main

import (
	"container/heap"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"slices"
	"sort"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/build/ninjago/chrometrace"
	"go.fuchsia.dev/fuchsia/tools/build/ninjago/compdb"
	"go.fuchsia.dev/fuchsia/tools/build/ninjago/ninjagraph"
	"go.fuchsia.dev/fuchsia/tools/build/ninjago/ninjalog"
	"go.fuchsia.dev/fuchsia/tools/build/ninjago/readerwriters"
	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

var (
	ninjaTracePath     = flag.String("ninjatrace", "", "path of ninjatrace.json file")
	ninjalogPath       = flag.String("ninjalog", "", "path of .ninja_log")
	compdbPath         = flag.String("compdb", "", "path of JSON compilation database")
	graphPath          = flag.String("graph", "", "path of graphviz dot file for ninja targets")
	outputPath         = flag.String("output", "", "path to output the serialized build stats")
	minActionBuildTime = flag.Duration("min_action_build_time", 5*time.Second, "actions that took longer than or equal to this time granularity are included in output")

	colors color.EnableColor
	level  logger.LogLevel
)

func init() {
	colors = color.ColorAuto
	level = logger.ErrorLevel
	flag.Var(&colors, "color", "use color in output, can be never, auto, always")
	flag.Var(&level, "level", "output verbosity, can be fatal, error, warning, info, debug or trace")
}

type inputs struct {
	ninjalog, compdb, graph io.Reader
}

// action describes a build action.
//
// All fields are exported so this struct can be serialized by json.
type action struct {
	Command    string
	Outputs    []string
	Start, End time.Duration
	Rule       string
	Category   string
	// TotalFloat is the amount of time this step can be delayed without affecting
	// the completion time of the build.
	//
	// https://en.wikipedia.org/wiki/Float_(project_management)
	TotalFloat time.Duration
	// Drag is the amount of time this step is adding to the total build time. All
	// non-critial steps have zero drag.
	//
	// https://en.wikipedia.org/wiki/Critical_path_drag
	Drag time.Duration
}

// All fields are exported so this struct can be serialized by json.
type catBuildTime struct {
	// Name of this category.
	Category string
	// Number of actions in this category.
	Count int32
	// The sum of build times spent for all actions in this category.
	BuildTime time.Duration
	// Build time of the fastest and slowest action in this category.
	MinBuildTime, MaxBuildTime time.Duration
}

// All fields are exported so this struct can be serialized by json.
type buildStats struct {
	// CriticalPath is the build path that takes the longest time to finish.
	CriticalPath []action
	// Slowests includes the slowest 30 builds actions.
	Slowests []action
	// CatBuildTimes groups build times by category.
	CatBuildTimes []catBuildTime
	// Sum of build times of all actions.
	TotalBuildTime time.Duration
	// Wall time spent to complete this build.
	BuildDuration time.Duration
	// All build actions from this build that took longer to finish than granularity.
	// TODO(jayzhuang): deprecate `All` after the pipeline is migrated to `Actions`.
	All []action
	// All build actions from this build that took longer to finish than granularity.
	Actions []action
}

// constructGraph constructs a ninjagraph based on files from input paths, and
// populates it with information from ninjalog and compdb.
//
// Steps used to populate the graph are also returned so they can be used in
// later steps.
func constructGraph(ins inputs) (ninjagraph.Graph, error) {
	njl, err := ninjalog.Parse(*ninjalogPath, ins.ninjalog)
	if err != nil {
		return ninjagraph.Graph{}, fmt.Errorf("parsing ninjalog: %v", err)
	}
	steps := ninjalog.Dedup(njl.Steps)

	commands, err := compdb.Parse(ins.compdb)
	if err != nil {
		return ninjagraph.Graph{}, fmt.Errorf("parsing compdb: %v", err)
	}
	steps = ninjalog.Populate(steps, commands)

	graph, err := ninjagraph.FromDOT(ins.graph)
	if err != nil {
		return ninjagraph.Graph{}, fmt.Errorf("parsing Ninja graph: %v", err)
	}
	if err := graph.PopulateEdges(steps); err != nil {
		return ninjagraph.Graph{}, fmt.Errorf("populating graph edges with build steps: %v", err)
	}
	graph, err = ninjagraph.WithStepsOnly(graph)
	if err != nil {
		return ninjagraph.Graph{}, fmt.Errorf("extracting partial graph: %v", err)
	}
	return graph, nil
}

type graph interface {
	PopulatedSteps() ([]ninjalog.Step, error)
}

func extractBuildStats(g graph, minActionBuildTime time.Duration) (buildStats, error) {
	steps, err := g.PopulatedSteps()
	if err != nil {
		return buildStats{}, fmt.Errorf("getting steps with float and drag: %w", err)
	}

	ret := buildStats{}
	for _, step := range steps {
		if step.OnCriticalPath {
			ret.CriticalPath = append(ret.CriticalPath, toAction(step))
		}
	}
	sort.Slice(ret.CriticalPath, func(i, j int) bool { return ret.CriticalPath[i].Start < ret.CriticalPath[j].Start })

	for _, step := range ninjalog.SlowestSteps(steps, 30) {
		ret.Slowests = append(ret.Slowests, toAction(step))
	}

	for _, step := range steps {
		if step.Duration() >= minActionBuildTime {
			ret.All = append(ret.All, toAction(step))
			ret.Actions = append(ret.Actions, toAction(step))
		}

		ret.TotalBuildTime += step.Duration()
		// The first action always starts at time zero, so build duration equals to
		// the end time of the last action.
		if step.End > ret.BuildDuration {
			ret.BuildDuration = step.End
		}
	}

	for _, stat := range ninjalog.StatsByType(steps, nil, func(s ninjalog.Step) string { return s.Category() }) {
		var minBuildTime, maxBuildTime time.Duration
		for i, t := range stat.Times {
			if i == 0 {
				minBuildTime, maxBuildTime = t, t
				continue
			}
			if t < minBuildTime {
				minBuildTime = t
			}
			if t > maxBuildTime {
				maxBuildTime = t
			}
		}
		ret.CatBuildTimes = append(ret.CatBuildTimes, catBuildTime{
			Category:     stat.Type,
			Count:        stat.Count,
			BuildTime:    stat.Time,
			MinBuildTime: minBuildTime,
			MaxBuildTime: maxBuildTime,
		})
	}
	return ret, nil
}

func toAction(s ninjalog.Step) action {
	a := action{
		Outputs:    append(s.Outs, s.Out),
		Start:      s.Start,
		End:        s.End,
		Category:   s.Category(),
		TotalFloat: s.TotalFloat,
		Drag:       s.Drag,
	}
	if s.Command != nil {
		a.Command = s.Command.Command
	}
	// TODO(jayzhuang): populate `Rule` when they are made available on step or
	// command.
	return a
}

func traceToOutputs(t *chrometrace.Trace) []string {
	var outputs []string
	if outputs_value, ok := t.Args["outputs"]; ok {
		for _, outputValue := range outputs_value.([]interface{}) {
			outputs = append(outputs, outputValue.(string))
		}
	}
	return outputs
}

func traceToAction(t *chrometrace.Trace) *action {
	var command string
	if commandValue, ok := t.Args["command"]; ok {
		command = commandValue.(string)
	}

	startTime := time.Duration(t.TimestampMicros) * time.Microsecond
	endTime := time.Duration(t.TimestampMicros+t.DurationMicros) * time.Microsecond

	var drag time.Duration
	eventCategories := strings.Split(t.Category, ",")
	if slices.Contains(eventCategories, "critical_path") {
		drag = endTime - startTime
	}

	var totalFloat time.Duration
	if totalFloatValue, ok := t.Args["total float"]; ok {
		// The "total float" value is in seconds, followed by "s"
		totalFloatDuration, err := time.ParseDuration(totalFloatValue.(string))
		if err != nil {
			log.Fatalf("Invalid total float value in chrome trace event: %q", totalFloatValue)
		}
		totalFloat = totalFloatDuration
	}

	return &action{
		Command:    command,
		Outputs:    traceToOutputs(t),
		Start:      startTime,
		End:        endTime,
		Category:   t.Category,
		TotalFloat: totalFloat,
		Drag:       drag,
	}
}

// traceMinHeap is a heap of `chrometrace.Trace` pointers. Its root is always the shortest
// trace in terms of duration.
type traceMinHeap []*chrometrace.Trace

func (h traceMinHeap) Len() int      { return len(h) }
func (h traceMinHeap) Swap(i, j int) { h[i], h[j] = h[j], h[i] }
func (h traceMinHeap) Less(i, j int) bool {
	return h[i].DurationMicros < h[j].DurationMicros
}
func (h *traceMinHeap) Push(x any) { *h = append(*h, x.(*chrometrace.Trace)) }
func (h *traceMinHeap) Pop() any {
	l := (*h).Len()
	if l == 0 {
		return nil
	}
	trace := (*h)[l-1]
	*h = (*h)[:l-1]
	return trace
}

// SlowestTraces returns the `n` traces that took the longest time to finish.
//
// Returned traces are sorted on build time in descending order (trace takes the
// longest time to build is the first element).
func slowestTraces(traces []*chrometrace.Trace, n int) []*chrometrace.Trace {
	mh := new(traceMinHeap)
	for _, trace := range traces {
		heap.Push(mh, trace)
		if mh.Len() > n {
			heap.Pop(mh)
		}
	}
	var res []*chrometrace.Trace
	for mh.Len() > 0 {
		res = append(res, heap.Pop(mh).(*chrometrace.Trace))
	}
	slices.Reverse(res)
	return res
}

// traceStat represents statistics for a trace event.
type traceStat struct {
	// Type used to group this stat, this is determined by the grouping function
	// provided by the caller when this stat is calculated.
	Type string
	// Count of builds for this type.
	Count int32
	// Accumulative build time for this type.
	Time time.Duration
	// Accumulative weighted build time for this time.
	Weighted time.Duration
	// Build times of all actions grouped under this stat.
	Times []time.Duration
}

// StatsByType summarizes build step statistics with weighted and typeOf.
// Order of the returned slice is undefined.
func statsByTraceType(traces []*chrometrace.Trace, weighted map[string]time.Duration, typeOf func(*chrometrace.Trace) string) []traceStat {
	if len(traces) == 0 {
		return nil
	}
	m := make(map[string]int) // type to index of stats.
	var stats []traceStat
	for _, trace := range traces {
		t := typeOf(trace)
		traceDuration := time.Duration(trace.DurationMicros) * time.Microsecond
		var traceOutput string
		outputs := traceToOutputs(trace)
		if len(outputs) > 0 {
			traceOutput = outputs[0]
		}
		if i, ok := m[t]; ok {
			stats[i].Count++
			stats[i].Time += traceDuration
			stats[i].Weighted += weighted[traceOutput]
			stats[i].Times = append(stats[i].Times, traceDuration)
			continue
		}
		stats = append(stats, traceStat{
			Type:     t,
			Count:    1,
			Time:     traceDuration,
			Times:    []time.Duration{traceDuration},
			Weighted: weighted[traceOutput],
		})
		m[t] = len(stats) - 1
	}
	return stats
}

// readChromeTrace reads an input chrome trace file, and returns a slice of pointers
// to individual trace events. Flow events are filtered out of the result.
func readChromeTrace(tracePath string) (result []*chrometrace.Trace, err error) {
	traceFile, err := readerwriters.Open(tracePath)
	if err != nil {
		return nil, fmt.Errorf("failed to read Ninja trace %q: %v", tracePath, err)
	}
	defer func() {
		if closeErr := traceFile.Close(); closeErr != nil && err == nil {
			err = closeErr
		}
	}()

	// Traces can be very large (e.g. > 500 MiB), reading them into an
	// array would require tremendous amount of heap reallocations, so
	// instead read each trace event individually and allocate a single
	// chrometrace.Trace instance for it in the heap.
	decoder := json.NewDecoder(traceFile)

	// Consume the initial opening list bracket
	_, err = decoder.Token()
	if err != nil {
		return nil, fmt.Errorf("error decoding opening bracket: %v", err)
	}

	for decoder.More() {
		var trace chrometrace.Trace
		if err := decoder.Decode(&trace); err != nil {
			return nil, fmt.Errorf("error decoding trace event %v", err)
		}
		if trace.EventType == chrometrace.FlowEventStart || trace.EventType == chrometrace.FlowEventEnd {
			continue
		}
		result = append(result, &trace)
	}

	// The closing bracket is optional in Chrome traces to allow
	// for unexpected generator crashes. Do not read it here.
	return
}

func extractBuildStatsFromTrace(ninjaTracePath string, minActionBuildTime time.Duration) (buildStats, error) {
	traces, err := readChromeTrace(ninjaTracePath)
	if err != nil {
		return buildStats{}, err
	}

	ret := buildStats{}
	for _, trace := range traces {
		if strings.Contains(trace.Category, "critical_path") {
			action := traceToAction(trace)
			if action != nil {
				ret.CriticalPath = append(ret.CriticalPath, *action)
			}
		}
	}
	sort.Slice(ret.CriticalPath, func(i, j int) bool { return ret.CriticalPath[i].Start < ret.CriticalPath[j].Start })

	for _, trace := range slowestTraces(traces, 30) {
		ret.Slowests = append(ret.Slowests, *traceToAction(trace))
	}

	for _, trace := range traces {
		traceDuration := time.Duration(trace.DurationMicros) * time.Microsecond
		if traceDuration >= minActionBuildTime {
			traceAction := *traceToAction(trace)
			ret.All = append(ret.All, traceAction)
			ret.Actions = append(ret.Actions, traceAction)
		}

		ret.TotalBuildTime += traceDuration
		// The first action always starts at time zero, so build duration equals to
		// the end time of the last action.
		traceEnd := time.Duration(trace.TimestampMicros+trace.DurationMicros) * time.Microsecond
		if traceEnd > ret.BuildDuration {
			ret.BuildDuration = traceEnd
		}
	}

	for _, stat := range statsByTraceType(traces, nil, func(t *chrometrace.Trace) string { return t.Category }) {
		var minBuildTime, maxBuildTime time.Duration
		for i, t := range stat.Times {
			if i == 0 {
				minBuildTime, maxBuildTime = t, t
				continue
			}
			if t < minBuildTime {
				minBuildTime = t
			}
			if t > maxBuildTime {
				maxBuildTime = t
			}
		}
		ret.CatBuildTimes = append(ret.CatBuildTimes, catBuildTime{
			Category:     stat.Type,
			Count:        stat.Count,
			BuildTime:    stat.Time,
			MinBuildTime: minBuildTime,
			MaxBuildTime: maxBuildTime,
		})
	}
	return ret, nil
}

func serializeBuildStats(s buildStats, w io.Writer) error {
	return json.NewEncoder(w).Encode(s)
}

func main() {
	flag.Parse()

	painter := color.NewColor(colors)
	log := logger.NewLogger(level, painter, os.Stdout, os.Stderr, "")

	if *ninjaTracePath == "" {
		if *ninjalogPath == "" {
			log.Fatalf("Either --ninjalog or --ninjatrace is required")
		}
		if *compdbPath == "" {
			log.Fatalf("--compdb is required")
		}
		if *graphPath == "" {
			log.Fatalf("--graph is required")
		}
	} else if *ninjalogPath != "" {
		log.Fatalf("--ninjalog cannot be used with --ninjatrace")
	} else if *compdbPath != "" {
		log.Fatalf("--compdb cannot be used with --ninjatrace")
	} else if *graphPath != "" {
		log.Fatalf("--graph cannot be used with --ninjatrace")
	}
	if *outputPath == "" {
		log.Fatalf("--output is required")
	}

	var stats buildStats

	if *ninjaTracePath != "" {
		extractedStats, err := extractBuildStatsFromTrace(*ninjaTracePath, *minActionBuildTime)
		if err != nil {
			log.Fatalf("Failed to extract build stats from trace %q: %v", *ninjaTracePath, err)
		}
		stats = extractedStats
	} else {
		log.Infof("Reading input files and constructing graph.")
		ninjalog, err := os.Open(*ninjalogPath)
		if err != nil {
			log.Fatalf("Failed to read Ninja log %q: %v", *ninjalogPath, err)
		}
		defer ninjalog.Close()
		compdb, err := os.Open(*compdbPath)
		if err != nil {
			log.Fatalf("Failed to read compdb %q: %v", *compdbPath, err)
		}
		defer compdb.Close()
		graphFile, err := os.Open(*graphPath)
		if err != nil {
			log.Fatalf("Failed to read graph %q: %v", *graphPath, err)
		}
		defer graphFile.Close()
		graph, err := constructGraph(inputs{
			ninjalog: ninjalog,
			compdb:   compdb,
			graph:    graphFile,
		})
		if err != nil {
			log.Fatalf("Failed to construct graph: %v", err)
		}

		log.Infof("Extracting build stats from graph.")
		extractedStats, err := extractBuildStats(&graph, *minActionBuildTime)
		if err != nil {
			log.Fatalf("Failed to extract build stats from graph: %v", err)
		}
		stats = extractedStats
	}

	log.Infof("Creating %s and serializing the build stats to it.", *outputPath)
	outputFile, err := readerwriters.Create(*outputPath)
	if err != nil {
		log.Fatalf("Failed to create output file: %v", err)
	}
	defer func() {
		if err := outputFile.Close(); err != nil {
			log.Errorf("Failed to close output file: %v", err)
		}
	}()

	if err := serializeBuildStats(stats, outputFile); err != nil {
		log.Fatalf("Failed to serialize build stats: %v", err)
	}
	log.Infof("Done.")
}
