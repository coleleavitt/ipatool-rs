package storeauth

// Records the Rust storeauth oracle fixture. Copied into a temporary export of the Go
// repository by scripts/storeauth-oracle.sh; not part of the Go tree.

import (
	"encoding/hex"
	"encoding/json"
	"os"
	"strings"
	"testing"

	"howett.net/plist"
)

type oracleIdentity struct {
	Identifier  string `json:"identifier"`
	LocalUserID string `json:"local_user_id"`
	DeviceID    string `json:"device_id"`
}

type oracleDecode struct {
	Input  string `json:"input"`
	Error  string `json:"error,omitempty"`
	Output string `json:"output,omitempty"`
}

type oracleHeaders struct {
	State   string            `json:"state"`
	OTP     string            `json:"otp"`
	MID     string            `json:"mid"`
	Headers map[string]string `json:"headers"`
}

type oraclePlist struct {
	Values map[string]any `json:"values"`
	Body   string         `json:"body"`
}

func TestRustOracle(t *testing.T) {
	out := os.Getenv("IPATOOL_ORACLE_OUT")
	if out == "" {
		t.Skip("IPATOOL_ORACLE_OUT is not set")
	}

	var fixture struct {
		Identities []oracleIdentity `json:"identities"`
		Decodes    []oracleDecode   `json:"decodes"`
		Marshals   []oracleDecode   `json:"marshals"`
		Headers    []oracleHeaders  `json:"headers"`
		Plists     []oraclePlist    `json:"plists"`
	}

	for _, id := range []string{"000102030405060708090a0b0c0d0e0f", "ffffffffffffffffffffffffffffffff", "8c1f2e3d4c5b6a798897a6b5c4d3e2f1"} {
		raw, _ := hex.DecodeString(id)
		lu, device := anisetteIdentityHeaders(raw)
		fixture.Identities = append(fixture.Identities, oracleIdentity{id, lu, device})
	}

	for _, input := range []string{
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Provisioning":"cHJvdmlzaW9uZWQ=","RoutingInfo":17106176}`,
		`{"identifier":"AAECAwQFBgcICQoLDA0ODw==","provisioning":"cHJvdmlzaW9uZWQ=","routinginfo":1}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Provisioning":null,"RoutingInfo":0}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Provisioning":"","RoutingInfo":0}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw=="}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Extra":[1,2],"RoutingInfo":18446744073709551615}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","RoutingInfo":18446744073709551616}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","RoutingInfo":-1}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","RoutingInfo":1.5}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","RoutingInfo":1e2}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","RoutingInfo":"1"}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw","RoutingInfo":1}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODx==","RoutingInfo":1}`,
		`{"Identifier":"AAECAwQF\nBgcICQoLDA0ODw==","RoutingInfo":1}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0O","RoutingInfo":1}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Identifier":"AAECAwQFBgcICQoLDA0ODxA="}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODxA=","identifier":"AAECAwQFBgcICQoLDA0ODw=="}`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw==","Provisioning":5}`,
		`[]`,
		`null`,
		`not json`,
		`{"Identifier":"AAECAwQFBgcICQoLDA0ODw=="} trailing`,
	} {
		entry := oracleDecode{Input: input}
		state, err := decodeAnisetteState([]byte(input))
		if err != nil {
			entry.Error = err.Error()
		} else {
			encoded, _ := json.Marshal(state)
			entry.Output = string(encoded)
		}
		fixture.Decodes = append(fixture.Decodes, entry)
	}

	identifier := []byte{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15}
	for _, state := range []anisetteState{
		{Identifier: identifier},
		{Identifier: identifier, Provisioning: []byte{}, RoutingInfo: 1},
		{Identifier: identifier, Provisioning: []byte("\x00\xff state"), RoutingInfo: ^uint64(0)},
	} {
		encoded, _ := json.Marshal(state)
		fixture.Marshals = append(fixture.Marshals, oracleDecode{Output: string(encoded)})
	}

	state := &anisetteState{Identifier: identifier, RoutingInfo: 17106176}
	headers := anisetteHeaders(state, []byte("otp"), []byte("mid"))
	flat := map[string]string{}
	for key, values := range headers {
		if key != "X-Apple-I-Client-Time" && key != "X-Apple-I-Timezone" {
			flat[key] = strings.Join(values, ",")
		}
	}
	encodedState, _ := json.Marshal(state)
	fixture.Headers = append(fixture.Headers, oracleHeaders{string(encodedState), "otp", "mid", flat})

	for _, values := range []map[string]any{
		{"Header": map[string]any{}, "Request": map[string]any{}},
		{"Header": map[string]any{}, "Request": map[string]any{"cpim": "Y3BpbQ=="}},
		{"clientData": "Y2xpZW50"},
		{"clientData": "Y2xpZW50", "kbsync": "a<b>&c\"d'e\tf\ng\rh"},
	} {
		body, err := plist.Marshal(values, plist.XMLFormat)
		if err != nil {
			t.Fatal(err)
		}
		fixture.Plists = append(fixture.Plists, oraclePlist{values, string(body)})
	}

	data, err := json.MarshalIndent(fixture, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
