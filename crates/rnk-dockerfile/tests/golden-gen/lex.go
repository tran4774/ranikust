package main

import (
	"fmt"
	"math/rand"
	"os"
	"sort"
	"strings"

	"github.com/moby/buildkit/frontend/dockerfile/shell"
)

// Shell lexer vectors: generated cases (a fixed seed, so the file is
// reproducible) and BuildKit's results for them, in one file.

type lexCase struct {
	Escape     string `json:"escape"`
	RawQuotes  bool   `json:"raw_quotes"`
	RawEscapes bool   `json:"raw_escapes"`
	SkipUnset  bool   `json:"skip_unset"`
	SkipQuotes bool   `json:"skip_quotes"`
	Word       string `json:"word"`
}

type lexVector struct {
	Case      lexCase  `json:"case"`
	Result    *string  `json:"result"`
	Words     []string `json:"words"`
	Matched   []string `json:"matched"`
	Unmatched []string `json:"unmatched"`
}

// lexEnv is the environment of every case; B is unset.
var lexEnv = map[string]string{
	"A": "abca",
	"C": "$0x",
	"D": "",
	"E": "a b  c",
	"F": "*/x/*.txt",
	"G": "a\nb",
}

type mapEnv map[string]string

func (m mapEnv) Get(k string) (string, bool) { v, ok := m[k]; return v, ok }
func (m mapEnv) Keys() []string {
	keys := []string{}
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func keys(m map[string]struct{}) []string {
	out := []string{}
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

var fragments = []string{
	"a", "b c", " ", "\t", "x", "é", "한", "\n",
	"$", "$A", "$B", "$E", "$G", "${A}", "${B}", "${E}", "$1", "$@", "$$", "$_x", "$A_", "${A_B}",
	"${A:-x}", "${B:-x y}", "${D:-d}", "${D-d}", "${A:+y}", "${B+z}", "${D+z}", "${A?m}", "${B?m}", "${D:?}",
	"${A#*a}", "${A##*a}", "${A%a*}", "${A%%a*}", "${A#?}", "${A%\\*}", "${F#*/}", "${F##*/}", "${F%/*}", "${F%%/*}",
	"${A/a/b}", "${A//a/b}", "${A//a/$C}", "${A/*/x}", "${A//?/x}", "${A///}", "${A//b}", "${F//\\*/s}", "${F/\\//_}",
	"${G//?/.}", "${G/*/.}", "${A//c/$}", "${A//b/\\$0}",
	"${A:-${B:-q}}", "${B:-\"q r\"}", "${A#\"a\"}", "${A#'a'}", "${#}", "${A:#x}", "${A:%x}", "${A=x}", "${A",
	"${", "}", "{", ":", "'", "\"", "'x y'", "\"x $A\"", "\"$E\"", "\"\\$A\"", "\"a\\\"b\"", "\"\\x\"",
	"\\", "\\$", "\\\"", "\\'", "\\\\", "\\ ", "`", "``", "`$A", "`\"", "<", "<<", "<< EOF", "<<-'E'", "3<<X",
	"*", "?", "/",
}

func genCases() []lexCase {
	r := rand.New(rand.NewSource(20261030))
	var cases []lexCase
	// Every fragment alone, with each escape character and default options.
	for _, f := range fragments {
		for _, esc := range []string{"\\", "`"} {
			cases = append(cases, lexCase{Escape: esc, Word: f})
		}
	}
	// Random concatenations with random options.
	for i := 0; i < 1500; i++ {
		var b strings.Builder
		n := 1 + r.Intn(5)
		for j := 0; j < n; j++ {
			b.WriteString(fragments[r.Intn(len(fragments))])
		}
		esc := "\\"
		if r.Intn(4) == 0 {
			esc = "`"
		}
		cases = append(cases, lexCase{
			Escape:     esc,
			RawQuotes:  r.Intn(3) == 0,
			RawEscapes: r.Intn(3) == 0,
			SkipUnset:  r.Intn(3) == 0,
			SkipQuotes: r.Intn(6) == 0,
			Word:       b.String(),
		})
	}
	return cases
}

func lexVectors(out string) {
	cases := genCases()
	vectors := make([]lexVector, 0, len(cases))
	env := mapEnv(lexEnv)
	for _, c := range cases {
		lex := shell.NewLex([]rune(c.Escape)[0])
		lex.RawQuotes = c.RawQuotes
		lex.RawEscapes = c.RawEscapes
		lex.SkipUnsetEnv = c.SkipUnset
		lex.SkipProcessQuotes = c.SkipQuotes
		v := lexVector{Case: c, Matched: []string{}, Unmatched: []string{}}
		res, err := lex.ProcessWordWithMatches(c.Word, env)
		if err == nil {
			s := res.Result
			v.Result = &s
			v.Matched = keys(res.Matched)
			v.Unmatched = keys(res.Unmatched)
		}
		words, err := lex.ProcessWords(c.Word, env)
		if err == nil {
			v.Words = strs(words)
		}
		vectors = append(vectors, v)
	}
	writeJSON(out, map[string]any{"env": lexEnv, "vectors": vectors})
	fmt.Printf("golden-gen: %d lexer vectors\n", len(vectors))
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: golden-gen dockerfile|expand <outdir> <file>... | golden-gen lex <out.json>")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "dockerfile":
		dockerfiles(os.Args[2], os.Args[3:])
	case "lex":
		lexVectors(os.Args[2])
	case "expand":
		expandGoldens(os.Args[2], os.Args[3:])
	default:
		fmt.Fprintln(os.Stderr, "unknown mode", os.Args[1])
		os.Exit(2)
	}
}
