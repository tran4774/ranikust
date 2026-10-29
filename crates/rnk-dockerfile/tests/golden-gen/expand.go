package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/moby/buildkit/client/llb/sourceresolver"
	"github.com/moby/buildkit/frontend/dockerfile/dockerfile2llb"
	"github.com/moby/buildkit/frontend/dockerui"
	"github.com/moby/buildkit/solver/pb"
	digest "github.com/opencontainers/go-digest"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
	"google.golang.org/protobuf/proto"
)

// Expansion goldens: BuildKit's dockerfile2llb converts each Dockerfile to
// LLB with a fake image resolver, and the result shows how build arguments,
// ARG and ENV were expanded: the target image's config and the environment
// and arguments of every RUN.

// baseImage is the config every resolved image gets.
var baseImage = map[string]any{
	"architecture": "amd64",
	"os":           "linux",
	"config": map[string]any{
		"Env":    []string{"PATH=/usr/local/bin:/usr/bin:/bin", "BASE_VAR=from-base"},
		"Labels": map[string]string{"base.label": "base"},
	},
	"rootfs": map[string]any{
		"type":     "layers",
		"diff_ids": []string{"sha256:0000000000000000000000000000000000000000000000000000000000000000"},
	},
}

type fakeResolver struct{}

func (fakeResolver) ResolveImageConfig(_ context.Context, ref string, _ sourceresolver.Opt) (string, digest.Digest, []byte, error) {
	dt, err := json.Marshal(baseImage)
	return ref, "", dt, err
}

// kanikoBuildArgs are the build arguments kaniko's integration tests pass
// (kaniko@44aa7ddc0 integration/images.go:143-159); mz1007 gets its base
// from the test itself (integration_test.go:2017).
var kanikoBuildArgs = map[string][]string{
	"Dockerfile_test_run":          {"file=/file"},
	"Dockerfile_test_run_new":      {"file=/file"},
	"Dockerfile_test_run_redo":     {"file=/file"},
	"Dockerfile_test_workdir":      {"workdir=/arg/workdir"},
	"Dockerfile_test_add":          {"file=context/foo"},
	"Dockerfile_test_arg_secret":   {"SSH_PRIVATE_KEY", "SSH_PUBLIC_KEY=Pµbl1cK€Y"},
	"Dockerfile_test_onbuild":      {"file=/tmp/onbuild"},
	"Dockerfile_test_scratch":      {"image=scratch", "hello=hello-value", "file=context/foo", "file3=context/b*"},
	"Dockerfile_test_multistage":   {"file=/foo2"},
	"Dockerfile_test_issue_mz655":  {"BASE_TAG=1.37.0"},
	"Dockerfile_test_issue_3373":   {"DIGEST=sha256:a680e90a30faae7b193edf63b088a11edf856f731d93c25f1fd7c1a21cf82d06"},
	"Dockerfile_test_issue_mz1007": {"BASE=127.0.0.2:5001/userb/test_issue_mz1007:base"},
}

type runOp struct {
	Args []string `json:"args"`
	Env  []string `json:"env"`
}

type expandOutput struct {
	File       string            `json:"file"`
	BuildArgs  map[string]string `json:"build_args"`
	OK         bool              `json:"ok"`
	Error      string            `json:"error,omitempty"`
	Env        []string          `json:"env"`
	Labels     map[string]string `json:"labels"`
	User       string            `json:"user"`
	Volumes    []string          `json:"volumes"`
	StopSignal string            `json:"stop_signal"`
	Runs       []runOp           `json:"runs"`
}

func expandOne(path string) expandOutput {
	name := filepath.Base(path)
	o := expandOutput{File: name, BuildArgs: map[string]string{}, Env: []string{}, Labels: map[string]string{}, Volumes: []string{}, Runs: []runOp{}}
	for _, a := range kanikoBuildArgs[name] {
		k, v, _ := strings.Cut(a, "=")
		o.BuildArgs[k] = v
	}
	dt, err := os.ReadFile(path)
	if err != nil {
		o.Error = err.Error()
		return o
	}
	amd64 := ocispecs.Platform{OS: "linux", Architecture: "amd64"}
	caps := pb.Caps.CapSet(pb.Caps.All())
	ctx := context.Background()
	res, err := dockerfile2llb.Dockerfile2LLB(ctx, dt, dockerfile2llb.ConvertOpt{
		Config: dockerui.Config{
			BuildArgs:      o.BuildArgs,
			BuildPlatforms: []ocispecs.Platform{amd64},
		},
		TargetPlatform: &amd64,
		MetaResolver:   fakeResolver{},
		LLBCaps:        &caps,
	})
	if err != nil {
		o.Error = err.Error()
		return o
	}
	cfg := res.Image.Config
	o.Env = strs(cfg.Env)
	if cfg.Labels != nil {
		o.Labels = cfg.Labels
	}
	o.User = cfg.User
	for v := range cfg.Volumes {
		o.Volumes = append(o.Volumes, v)
	}
	sort.Strings(o.Volumes)
	o.StopSignal = cfg.StopSignal

	def, err := res.State.Marshal(ctx)
	if err != nil {
		o.Error = err.Error()
		return o
	}
	for _, opdt := range def.Def {
		var op pb.Op
		if err := proto.Unmarshal(opdt, &op); err != nil {
			o.Error = err.Error()
			return o
		}
		if ex := op.GetExec(); ex != nil {
			env := append([]string{}, ex.Meta.Env...)
			sort.Strings(env)
			o.Runs = append(o.Runs, runOp{Args: strs(ex.Meta.Args), Env: env})
		}
	}
	sort.Slice(o.Runs, func(i, j int) bool {
		a, _ := json.Marshal(o.Runs[i])
		b, _ := json.Marshal(o.Runs[j])
		return string(a) < string(b)
	})
	o.OK = true
	return o
}

func expandGoldens(outdir string, files []string) {
	sort.Strings(files)
	ok := 0
	for _, f := range files {
		o := expandOne(f)
		if o.OK {
			ok++
		}
		writeJSON(filepath.Join(outdir, filepath.Base(f)+".json"), o)
	}
	fmt.Printf("golden-gen: %d/%d files convert to LLB\n", ok, len(files))
}
