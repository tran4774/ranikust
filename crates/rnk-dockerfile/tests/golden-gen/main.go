// golden-gen writes the BuildKit reference output that rnk-dockerfile's
// golden tests compare against. It calls BuildKit v0.34.0's own parser,
// instruction parser and shell lexer; see README.md for how to run it.
//
// Usage:
//
//	golden-gen dockerfile <outdir> <file>...   one <basename>.json per file
//	golden-gen expand <outdir> <file>...       ARG/ENV expansion through dockerfile2llb
//	golden-gen lex <out.json>                   shell lexer vectors
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/moby/buildkit/frontend/dockerfile/instructions"
	"github.com/moby/buildkit/frontend/dockerfile/parser"
)

// ---------------------------------------------------------------------------
// Dockerfiles

type heredoc struct {
	Name    string `json:"name"`
	FD      uint   `json:"fd"`
	Expand  bool   `json:"expand"`
	Chomp   bool   `json:"chomp"`
	Content string `json:"content"`
}

type astNode struct {
	Dump     string    `json:"dump"`
	Lines    [2]int    `json:"lines"`
	Heredocs []heredoc `json:"heredocs"`
}

type stage struct {
	Name     string           `json:"name"`
	Base     string           `json:"base"`
	Platform string           `json:"platform"`
	Lines    [2]int           `json:"lines"`
	Commands []map[string]any `json:"commands"`
}

type output struct {
	File      string           `json:"file"`
	OK        bool             `json:"ok"`
	Error     string           `json:"error,omitempty"`
	ErrorLine int              `json:"error_line,omitempty"`
	Escape    string           `json:"escape,omitempty"`
	Warnings  int              `json:"warnings"`
	AST       []astNode        `json:"ast"`
	MetaArgs  []map[string]any `json:"meta_args"`
	Stages    []stage          `json:"stages"`
}

func lines(r []parser.Range) [2]int {
	if len(r) == 0 {
		return [2]int{0, 0}
	}
	return [2]int{r[0].Start.Line, r[len(r)-1].End.Line}
}

func errorLine(err error) int {
	var le *parser.LocationError
	if errors.As(err, &le) && len(le.Locations) > 0 && len(le.Locations[0]) > 0 {
		return le.Locations[0][0].Start.Line
	}
	return 0
}

func optBool(b *bool) any {
	if b == nil {
		return nil
	}
	return *b
}

func optUint(v *uint64) any {
	if v == nil {
		return nil
	}
	return *v
}

func optString(v *string) any {
	if v == nil {
		return nil
	}
	return *v
}

func strs(v []string) []string {
	if v == nil {
		return []string{}
	}
	return v
}

func shellDependant(m map[string]any, c instructions.ShellDependantCmdLine) {
	m["prepend_shell"] = c.PrependShell
	m["cmdline"] = strs(c.CmdLine)
	files := [][]any{}
	for _, f := range c.Files {
		files = append(files, []any{f.Name, f.Data, f.Chomp})
	}
	m["files"] = files
}

func sourcesAndDest(m map[string]any, s instructions.SourcesAndDest) {
	m["sources"] = strs(s.SourcePaths)
	m["dest"] = s.DestPath
	contents := [][]any{}
	for _, c := range s.SourceContents {
		contents = append(contents, []any{c.Path, c.Data, c.Expand})
	}
	m["contents"] = contents
}

func command(cmd instructions.Command) map[string]any {
	m := map[string]any{
		"cmd":   strings.ToLower(cmd.Name()),
		"lines": lines(cmd.Location()),
	}
	switch c := cmd.(type) {
	case *instructions.EnvCommand:
		pairs := [][]any{}
		for _, p := range c.Env {
			pairs = append(pairs, []any{p.Key, p.Value, p.NoDelim})
		}
		m["pairs"] = pairs
	case *instructions.LabelCommand:
		pairs := [][]any{}
		for _, p := range c.Labels {
			pairs = append(pairs, []any{p.Key, p.Value, p.NoDelim})
		}
		m["pairs"] = pairs
	case *instructions.ArgCommand:
		args := [][]any{}
		for _, a := range c.Args {
			args = append(args, []any{a.Key, optString(a.Value)})
		}
		m["args"] = args
	case *instructions.MaintainerCommand:
		m["value"] = c.Maintainer
	case *instructions.WorkdirCommand:
		m["value"] = c.Path
	case *instructions.UserCommand:
		m["value"] = c.User
	case *instructions.StopSignalCommand:
		m["value"] = c.Signal
	case *instructions.ExposeCommand:
		m["values"] = strs(c.Ports)
	case *instructions.VolumeCommand:
		m["values"] = strs(c.Volumes)
	case *instructions.ShellCommand:
		m["values"] = strs(c.Shell)
	case *instructions.OnbuildCommand:
		m["expression"] = c.Expression
	case *instructions.CmdCommand:
		shellDependant(m, c.ShellDependantCmdLine)
	case *instructions.EntrypointCommand:
		shellDependant(m, c.ShellDependantCmdLine)
	case *instructions.RunCommand:
		shellDependant(m, c.ShellDependantCmdLine)
		used := append([]string{}, c.FlagsUsed...)
		sort.Strings(used)
		m["flags_used"] = used
		m["network"] = instructions.GetNetwork(c)
		m["security"] = instructions.GetSecurity(c)
		devices := [][]any{}
		for _, d := range instructions.GetDevices(c) {
			devices = append(devices, []any{d.Name, d.Required})
		}
		m["devices"] = devices
		// Parse the mounts completely, as the dispatcher does, but with an
		// expander that changes nothing.
		if err := c.Expand(func(w string) (string, error) { return w, nil }); err != nil {
			m["mount_error"] = err.Error()
		} else {
			mounts := []map[string]any{}
			for _, mt := range instructions.GetMounts(c) {
				mounts = append(mounts, map[string]any{
					"type":     string(mt.Type),
					"from":     mt.From,
					"source":   mt.Source,
					"target":   mt.Target,
					"readonly": mt.ReadOnly,
					"size":     mt.SizeLimit,
					"id":       mt.CacheID,
					"sharing":  string(mt.CacheSharing),
					"required": mt.Required,
					"env":      optString(mt.Env),
					"mode":     optUint(mt.Mode),
					"uid":      optUint(mt.UID),
					"gid":      optUint(mt.GID),
				})
			}
			m["mounts"] = mounts
		}
	case *instructions.CopyCommand:
		sourcesAndDest(m, c.SourcesAndDest)
		m["from"] = c.From
		m["chown"] = c.Chown
		m["chmod"] = c.Chmod
		m["link"] = c.Link
		m["parents"] = c.Parents
		m["excludes"] = strs(c.ExcludePatterns)
	case *instructions.AddCommand:
		sourcesAndDest(m, c.SourcesAndDest)
		m["chown"] = c.Chown
		m["chmod"] = c.Chmod
		m["link"] = c.Link
		m["excludes"] = strs(c.ExcludePatterns)
		m["keep_git_dir"] = optBool(c.KeepGitDir)
		m["checksum"] = c.Checksum
		m["unpack"] = optBool(c.Unpack)
	case *instructions.HealthCheckCommand:
		h := c.Health
		m["test"] = strs(h.Test)
		m["interval"] = int64(h.Interval)
		m["timeout"] = int64(h.Timeout)
		m["start_period"] = int64(h.StartPeriod)
		m["start_interval"] = int64(h.StartInterval)
		m["retries"] = h.Retries
	default:
		m["unknown_type"] = fmt.Sprintf("%T", cmd)
	}
	return m
}

func one(path string) output {
	o := output{File: filepath.Base(path), AST: []astNode{}, MetaArgs: []map[string]any{}, Stages: []stage{}}
	data, err := os.ReadFile(path)
	if err != nil {
		o.Error = err.Error()
		return o
	}
	res, err := parser.Parse(bytes.NewReader(data))
	if err != nil {
		o.Error = err.Error()
		o.ErrorLine = errorLine(err)
		return o
	}
	o.Escape = string(res.EscapeToken)
	o.Warnings = len(res.Warnings)
	for _, n := range res.AST.Children {
		an := astNode{Dump: n.Dump(), Lines: [2]int{n.StartLine, n.EndLine}, Heredocs: []heredoc{}}
		for _, h := range n.Heredocs {
			an.Heredocs = append(an.Heredocs, heredoc{h.Name, h.FileDescriptor, h.Expand, h.Chomp, h.Content})
		}
		o.AST = append(o.AST, an)
	}
	stages, metaArgs, err := instructions.Parse(res.AST, nil)
	if err != nil {
		o.Error = err.Error()
		o.ErrorLine = errorLine(err)
		return o
	}
	for i := range metaArgs {
		o.MetaArgs = append(o.MetaArgs, command(&metaArgs[i]))
	}
	for _, st := range stages {
		s := stage{Name: st.Name, Base: st.BaseName, Platform: st.Platform, Lines: lines(st.Location), Commands: []map[string]any{}}
		for _, c := range st.Commands {
			s.Commands = append(s.Commands, command(c))
		}
		o.Stages = append(o.Stages, s)
	}
	o.OK = true
	return o
}

func writeJSON(path string, v any) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(v); err != nil {
		panic(err)
	}
	if err := os.WriteFile(path, buf.Bytes(), 0o644); err != nil {
		panic(err)
	}
}

func dockerfiles(outdir string, files []string) {
	sort.Strings(files)
	ok := 0
	for _, f := range files {
		o := one(f)
		if o.OK {
			ok++
		}
		writeJSON(filepath.Join(outdir, filepath.Base(f)+".json"), o)
	}
	fmt.Printf("golden-gen: %d/%d files parse OK\n", ok, len(files))
}

