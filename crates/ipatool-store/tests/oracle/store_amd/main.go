// Command store-amd-oracle records the Store AMD flow of the Go emulator for the Rust port.
//
// usage: store-amd-oracle <anisette-v4 state file> <output file>
//
// The replay record runs the deterministic (patched) emulator with the real modern ADI and
// records each com.apple.adid XPC exchange, the headers, the call trace and the heap size.
// The storeauth record runs Go's unpatched StoreAMDHeaders entry point on the same state and
// keeps only the header shape and how the state changed.
package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"os"

	"github.com/majd/ipatool/v2/internal/sap/assets"
	"github.com/majd/ipatool/v2/internal/sap/machine"
	"github.com/majd/ipatool/v2/internal/store/emulator"
	"github.com/majd/ipatool/v2/internal/store/profile"
	"github.com/majd/ipatool/v2/internal/storeauth"
)

const endpoint = "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate"

var hardwareID = []byte{0x3c, 0x22, 0xfb, 0x12, 0x34, 0x56}

type state struct {
	Identifier   []byte
	Provisioning []byte
	RoutingInfo  uint64
}

type exchange struct {
	Service string `json:"service"`
	Request []byte `json:"request"`
	Reply   []byte `json:"reply"`
}

type shape struct {
	Headers             map[string]int `json:"headers"`
	Base64              map[string]bool `json:"base64"`
	Error               string         `json:"error"`
	IdentifierKept      bool           `json:"identifier_kept"`
	RoutingInfoKept     bool           `json:"routing_info_kept"`
	ProvisioningChanged bool           `json:"provisioning_changed"`
	ProvisioningLength  int            `json:"provisioning_length"`
	Keys                []string       `json:"keys"`
}

type record struct {
	Endpoint   string            `json:"endpoint"`
	HardwareID []byte            `json:"hardware_id"`
	Exchanges  []exchange        `json:"exchanges"`
	Headers    map[string]string `json:"headers"`
	Error      string            `json:"error"`
	Trace      []string          `json:"trace"`
	Cursor     uint64            `json:"cursor"`
	Storeauth  shape             `json:"storeauth"`
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func headerShape(headers map[string]string) (map[string]int, map[string]bool) {
	lengths, encoded := map[string]int{}, map[string]bool{}
	for key, value := range headers {
		lengths[key] = len(value)
		_, err := base64.StdEncoding.DecodeString(value)
		encoded[key] = err == nil
	}

	return lengths, encoded
}

func main() {
	ctx := context.Background()
	raw, err := os.ReadFile(os.Args[1])
	must(err)

	var saved state
	must(json.Unmarshal(raw, &saved))

	bundle, err := assets.Load(ctx)
	must(err)
	prepared, err := profile.Load(ctx)
	must(err)

	out := record{Endpoint: endpoint, HardwareID: hardwareID}
	adi, err := machine.OpenModernADI(ctx, bundle, saved.Identifier, saved.Provisioning)
	must(err)
	framework, err := emulator.Open(ctx, bundle.AppleMediaServices, prepared)
	must(err)
	out.Headers, err = framework.StoreAMDHeaders(endpoint, hardwareID, func(service string, request []byte) ([]byte, error) {
		reply, err := adi.ExchangeXPC(request)
		out.Exchanges = append(out.Exchanges, exchange{Service: service, Request: request, Reply: reply})

		return reply, err
	})
	if err != nil {
		out.Error = err.Error()
	}
	out.Trace = emulator.OracleTrace
	out.Cursor = emulator.OracleCursor(framework)
	_ = framework.Close()
	_ = adi.Close()

	headers, updated, err := storeauth.StoreAMDHeaders(ctx, raw, endpoint, hardwareID)
	if err != nil {
		out.Storeauth.Error = err.Error()
	} else {
		out.Storeauth.Headers, out.Storeauth.Base64 = headerShape(headers)
		var next state
		must(json.Unmarshal(updated, &next))
		var keys map[string]json.RawMessage
		must(json.Unmarshal(updated, &keys))
		for key := range keys {
			out.Storeauth.Keys = append(out.Storeauth.Keys, key)
		}
		out.Storeauth.IdentifierKept = bytes.Equal(next.Identifier, saved.Identifier)
		out.Storeauth.RoutingInfoKept = next.RoutingInfo == saved.RoutingInfo
		out.Storeauth.ProvisioningChanged = !bytes.Equal(next.Provisioning, saved.Provisioning)
		out.Storeauth.ProvisioningLength = len(next.Provisioning)
	}

	encoded, err := json.MarshalIndent(out, "", "  ")
	must(err)
	must(os.WriteFile(os.Args[2], encoded, 0o600))
}
