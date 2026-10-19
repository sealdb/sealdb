package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"reflect"
	"strings"

	"github.com/pingcap/tidb/pkg/parser"
	"github.com/pingcap/tidb/pkg/parser/format"
	_ "github.com/pingcap/tidb/pkg/parser/test_driver"
)

type errorOut struct {
	Code    int    `json:"code,omitempty"`
	Message string `json:"message"`
}

type stmtOut struct {
	Type    string `json:"type"`
	Restore string `json:"restore,omitempty"`
	Text    string `json:"text,omitempty"`
}

type result struct {
	SQL        string    `json:"sql"`
	Statements []stmtOut `json:"statements"`
	Warnings   []string  `json:"warnings,omitempty"`
	Error      *errorOut `json:"error,omitempty"`
}

type caseOut struct {
	Name   string `json:"name"`
	Result result `json:"result"`
}

type suiteOut struct {
	Suite string    `json:"suite"`
	Cases []caseOut `json:"cases"`
}

func restore(stmt interface {
	Restore(*format.RestoreCtx) error
}) (string, error) {
	var sb strings.Builder
	ctx := format.NewRestoreCtx(format.DefaultRestoreFlags, &sb)
	if err := stmt.Restore(ctx); err != nil {
		return "", err
	}
	return sb.String(), nil
}

func parseSQL(sql string) result {
	out := result{SQL: sql, Statements: []stmtOut{}}
	p := parser.New()
	stmts, warns, err := p.ParseSQL(sql)
	if err != nil {
		out.Error = &errorOut{Message: err.Error()}
		if te, ok := err.(interface{ Code() int }); ok {
			out.Error.Code = te.Code()
		}
	}
	for _, w := range warns {
		out.Warnings = append(out.Warnings, w.Error())
	}
	for _, st := range stmts {
		typ := reflect.TypeOf(st)
		name := ""
		if typ.Kind() == reflect.Ptr {
			name = typ.Elem().Name()
		} else {
			name = typ.Name()
		}
		item := stmtOut{Type: name}
		if restored, rerr := restore(st); rerr == nil {
			item.Restore = restored
		}
		if st.Text() != "" {
			item.Text = st.Text()
		}
		out.Statements = append(out.Statements, item)
	}
	return out
}

func main() {
	sqlFlag := flag.String("sql", "", "SQL text to parse")
	fileFlag := flag.String("file", "", "file containing SQL text")
	suiteFlag := flag.String("suite", "", "directory containing *.sql cases")
	outFlag := flag.String("out", "", "output JSON file (default stdout)")
	prettyFlag := flag.Bool("pretty", false, "pretty-print JSON")
	flag.Parse()

	if *suiteFlag != "" {
		entries, err := os.ReadDir(*suiteFlag)
		if err != nil {
			fmt.Fprintf(os.Stderr, "read suite: %v\n", err)
			os.Exit(2)
		}
		out := suiteOut{Suite: *suiteFlag, Cases: []caseOut{}}
		for _, e := range entries {
			if e.IsDir() || !strings.HasSuffix(e.Name(), ".sql") {
				continue
			}
			data, err := os.ReadFile(*suiteFlag + "/" + e.Name())
			if err != nil {
				fmt.Fprintf(os.Stderr, "read case: %v\n", err)
				os.Exit(2)
			}
			out.Cases = append(out.Cases, caseOut{Name: e.Name(), Result: parseSQL(string(data))})
		}
		var data []byte
		if *prettyFlag {
			data, err = json.MarshalIndent(out, "", "  ")
		} else {
			data, err = json.Marshal(out)
		}
		if err != nil {
			fmt.Fprintf(os.Stderr, "marshal: %v\n", err)
			os.Exit(2)
		}
		data = append(data, '\n')
		if *outFlag != "" {
			if err := os.WriteFile(*outFlag, data, 0o644); err != nil {
				fmt.Fprintf(os.Stderr, "write file: %v\n", err)
				os.Exit(2)
			}
			return
		}
		os.Stdout.Write(data)
		return
	}

	sql := *sqlFlag
	if *fileFlag != "" {
		data, err := os.ReadFile(*fileFlag)
		if err != nil {
			fmt.Fprintf(os.Stderr, "read file: %v\n", err)
			os.Exit(2)
		}
		sql = string(data)
	}
	if sql == "" {
		fmt.Fprintln(os.Stderr, "usage: tidb-oracle -sql \"select 1\" | -file in.sql | -suite dir [-pretty] [-out file]")
		os.Exit(2)
	}

	out := parseSQL(sql)
	var data []byte
	var err error
	if *prettyFlag {
		data, err = json.MarshalIndent(out, "", "  ")
	} else {
		data, err = json.Marshal(out)
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "marshal: %v\n", err)
		os.Exit(2)
	}
	data = append(data, '\n')

	if *outFlag != "" {
		if err := os.WriteFile(*outFlag, data, 0o644); err != nil {
			fmt.Fprintf(os.Stderr, "write file: %v\n", err)
			os.Exit(2)
		}
		return
	}
	os.Stdout.Write(data)
}
