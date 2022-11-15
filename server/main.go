package main

import (
	"fmt"
	"github.com/sealdb/seal/sqlparser"
	"github.com/sealdb/seal/version"
)

const banner string = `
          _____                    _____                    _____                    _____            _____                    _____          
         /\    \                  /\    \                  /\    \                  /\    \          /\    \                  /\    \         
        /::\    \                /::\    \                /::\    \                /::\____\        /::\    \                /::\    \        
       /::::\    \              /::::\    \              /::::\    \              /:::/    /       /::::\    \              /::::\    \       
      /::::::\    \            /::::::\    \            /::::::\    \            /:::/    /       /::::::\    \            /::::::\    \      
     /:::/\:::\    \          /:::/\:::\    \          /:::/\:::\    \          /:::/    /       /:::/\:::\    \          /:::/\:::\    \     
    /:::/__\:::\    \        /:::/__\:::\    \        /:::/__\:::\    \        /:::/    /       /:::/  \:::\    \        /:::/__\:::\    \    
    \:::\   \:::\    \      /::::\   \:::\    \      /::::\   \:::\    \      /:::/    /       /:::/    \:::\    \      /::::\   \:::\    \   
  ___\:::\   \:::\    \    /::::::\   \:::\    \    /::::::\   \:::\    \    /:::/    /       /:::/    / \:::\    \    /::::::\   \:::\    \  
 /\   \:::\   \:::\    \  /:::/\:::\   \:::\    \  /:::/\:::\   \:::\    \  /:::/    /       /:::/    /   \:::\ ___\  /:::/\:::\   \:::\ ___\ 
/::\   \:::\   \:::\____\/:::/__\:::\   \:::\____\/:::/  \:::\   \:::\____\/:::/____/       /:::/____/     \:::|    |/:::/__\:::\   \:::|    |
\:::\   \:::\   \::/    /\:::\   \:::\   \::/    /\::/    \:::\  /:::/    /\:::\    \       \:::\    \     /:::|____|\:::\   \:::\  /:::|____|
 \:::\   \:::\   \/____/  \:::\   \:::\   \/____/  \/____/ \:::\/:::/    /  \:::\    \       \:::\    \   /:::/    /  \:::\   \:::\/:::/    / 
  \:::\   \:::\    \       \:::\   \:::\    \               \::::::/    /    \:::\    \       \:::\    \ /:::/    /    \:::\   \::::::/    /  
   \:::\   \:::\____\       \:::\   \:::\____\               \::::/    /      \:::\    \       \:::\    /:::/    /      \:::\   \::::/    /   
    \:::\  /:::/    /        \:::\   \::/    /               /:::/    /        \:::\    \       \:::\  /:::/    /        \:::\  /:::/    /    
     \:::\/:::/    /          \:::\   \/____/               /:::/    /          \:::\    \       \:::\/:::/    /          \:::\/:::/    /     
      \::::::/    /            \:::\    \                  /:::/    /            \:::\    \       \::::::/    /            \::::::/    /      
       \::::/    /              \:::\____\                /:::/    /              \:::\____\       \::::/    /              \::::/    /       
        \::/    /                \::/    /                \::/    /                \::/    /        \::/____/                \::/____/        
         \/____/                  \/____/                  \/____/                  \/____/          ~~                       ~~
`

func main() {
	fmt.Println(banner)
	fmt.Printf("version: [%+v]\n", version.GetVersion())

	testcases := []struct {
		input  string
		output string
		err    error
	}{
		{
			input:  "select 1",
			output: "select 1 from dual",
		}, {
			input: "",
			err:   sqlparser.ErrEmpty,
		}, {
			input: ";",
			err:   sqlparser.ErrEmpty,
		}, {
			input:  "-- sdf",
			output: "-- sdf",
		}, {
			input:  "/* sdf */",
			output: "/* sdf */",
		}, {
			input:  "# sdf",
			output: "# sdf",
		}, {
			input:  "/* sdf */ select 1",
			output: "select 1 from dual",
		},
	}
	for _, testcase := range testcases {
		res, err := sqlparser.Parse(testcase.input)
		fmt.Printf("testcase.err: %v, err: %v\n", testcase.err, err)
		fmt.Printf("output: %v, res_str: %s\n", testcase.output, sqlparser.String(res))

		res, err = sqlparser.ParseStrictDDL(testcase.input)
		fmt.Printf("testcase.err: %v, err: %v\n", testcase.err, err)
		fmt.Printf("output: %v, res_str: %s\n", testcase.output, sqlparser.String(res))
	}
}
