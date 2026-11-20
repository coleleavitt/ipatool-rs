package main

import (
	"fmt"
	"os"

	"github.com/byteness/keyring"
)

// usage: kr-interop <dir> get|set <key> [data]
func main() {
	ring, err := keyring.Open(keyring.Config{
		AllowedBackends:  []keyring.BackendType{keyring.FileBackend},
		ServiceName:      "ipatool-auth.service",
		FileDir:          os.Args[1],
		FilePasswordFunc: keyring.FixedStringPrompt("test-passphrase"),
	})
	if err != nil {
		panic(err)
	}
	switch os.Args[2] {
	case "get":
		item, err := ring.Get(os.Args[3])
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		fmt.Printf("%s|%s\n", item.Data, item.Label)
	case "set":
		if err := ring.Set(keyring.Item{Key: os.Args[3], Data: []byte(os.Args[4]), Label: "ipatool-auth.service"}); err != nil {
			panic(err)
		}
	}
}
