package tp

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/pingcap/tidb/pkg/domain"
	"github.com/pingcap/tidb/pkg/statistics/handle/util"
	"github.com/pingcap/tidb/pkg/testkit"
	"github.com/pingcap/tidb/pkg/testkit/testsetup"
)

func TestMain(m *testing.M) {
	testsetup.SetupForCommonTest()
	os.Exit(m.Run())
}

// loadStats injects a corpus stats JSON through the stats handle. The SQL
// `LOAD STATS` statement cannot be used here: it hands the work to the session
// via a session value that only the binary's execute path drains, so a testkit
// session reports "previous load stats option isn't closed normally".
func loadStats(tk *testkit.TestKit, path string) error {
	bs, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	jsonTbl := &util.JSONTable{}
	if err := json.Unmarshal(bs, jsonTbl); err != nil {
		return err
	}
	// Corpus JSON carries the suite database name; landing it in the probe's
	// current database keeps the file reusable as-is.
	jsonTbl.DatabaseName = tk.Session().GetSessionVars().CurrentDB
	dom := domain.GetDomain(tk.Session())
	return dom.StatsHandle().LoadStatsFromJSON(context.Background(), dom.InfoSchema(), jsonTbl, 0)
}

// Run the statements in $PROBE_FILE: rows print tab-separated.
// Statements are separated by ";\n"; lines starting with "-- " are comments.
// A statement `loadstats <path>` injects stats from a corpus JSON file.
func TestProbe(t *testing.T) {
	path := os.Getenv("PROBE_FILE")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	tk := testkit.NewTestKit(t, testkit.CreateMockStore(t))
	tk.MustExec("use test")
	for _, stmt := range strings.Split(string(data), ";\n") {
		stmt = strings.TrimSpace(stmt)
		if stmt == "" || strings.HasPrefix(stmt, "--") {
			continue
		}
		fmt.Printf(">>> %s\n", stmt)
		if rest, ok := strings.CutPrefix(stmt, "loadstats "); ok {
			if err := loadStats(tk, strings.TrimSpace(rest)); err != nil {
				t.Fatalf("%s: %v", stmt, err)
			}
			continue
		}
		rs, err := tk.Exec(stmt)
		if err != nil {
			t.Fatalf("%s: %v", stmt, err)
		}
		if rs == nil {
			continue
		}
		for _, row := range tk.ResultSetToResult(rs, stmt).Rows() {
			parts := make([]string, len(row))
			for i, v := range row {
				parts[i] = fmt.Sprint(v)
			}
			fmt.Println(strings.Join(parts, "\t"))
		}
	}
}
