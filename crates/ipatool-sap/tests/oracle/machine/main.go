// Command machine-oracle records Go internal/sap/machine outputs for the Rust port's oracle
// test: the SAP setup and signing flow used by login (against Apple's live setup endpoints),
// kbsync, and the modern ADI runtime. Randomness and wall time are made deterministic by
// scripts/machine-oracle.sh, which patches a temporary export of the Go tree.
package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"time"

	"howett.net/plist"

	"github.com/majd/ipatool/v2/internal/sap/assets"
	"github.com/majd/ipatool/v2/internal/sap/machine"
)

const (
	userAgent      = "Configurator/2.17 (Macintosh; OS X 15.2; 24C5089c) AppleWebKit/0620.1.16.11.6"
	certificateURL = "https://s.mzstatic.com/sap/setupCert.plist"
	setupURL       = "https://fpinit.itunes.apple.com/v1/signSapSetup/legacy"
)

var (
	hardwareID = []byte{0x02, 0x11, 0x22, 0x33, 0x44, 0x55}
	hostUUID   = []byte("ipatool-oracle-1")
	kbsyncDSID = uint64(12345678901)
	signInputs = []string{"", "61", hex.EncodeToString(bytes.Repeat([]byte("ipatool"), 150))}
)

type sign struct {
	Input  string `json:"input"`
	Output string `json:"output"`
}

type record struct {
	HardwareID   string `json:"hardware_id"`
	Context      uint64 `json:"context"`
	Certificate  string `json:"certificate"`
	Request      string `json:"request"`
	RequestState int32  `json:"request_state"`
	Reply        string `json:"reply"`
	Final        string `json:"final"`
	FinalState   int32  `json:"final_state"`
	Signs        []sign `json:"signs"`
	KBSyncDSID   uint64 `json:"kbsync_dsid"`
	KBSync       string `json:"kbsync"`
	HostUUID     string `json:"host_uuid"`
	ADIOTP       string `json:"adi_otp"`
	SignNanos    int64  `json:"sign_nanos"`
	FlowNanos    int64  `json:"flow_nanos"`
}

func main() {
	if err := run(os.Args[1]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func fetch(request *http.Request, key string) ([]byte, error) {
	request.Header.Set("User-Agent", userAgent)
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, err
	}
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("apple returned %s", response.Status)
	}
	var values map[string]any
	if _, err := plist.Unmarshal(body, &values); err != nil {
		return nil, err
	}
	value, ok := values[key].([]byte)
	if !ok {
		return nil, fmt.Errorf("missing %s", key)
	}
	return value, nil
}

func run(output string) error {
	ctx := context.Background()
	bundle, err := assets.Load(ctx)
	if err != nil {
		return err
	}
	r := record{HardwareID: hex.EncodeToString(hardwareID), KBSyncDSID: kbsyncDSID, HostUUID: hex.EncodeToString(hostUUID)}

	start := time.Now()
	guest, err := machine.Open(ctx, bundle)
	if err != nil {
		return err
	}
	if r.Context, err = guest.Initialize(hardwareID); err != nil {
		return err
	}
	get, _ := http.NewRequest(http.MethodGet, certificateURL, nil)
	certificate, err := fetch(get, "sign-sap-setup-cert")
	if err != nil {
		return err
	}
	r.Certificate = hex.EncodeToString(certificate)
	request, state, err := guest.Exchange(200, hardwareID, r.Context, certificate)
	if err != nil {
		return err
	}
	r.Request, r.RequestState = hex.EncodeToString(request), state
	envelope, err := plist.Marshal(map[string]any{"sign-sap-setup-buffer": request}, plist.XMLFormat)
	if err != nil {
		return err
	}
	post, _ := http.NewRequest(http.MethodPost, setupURL, bytes.NewReader(envelope))
	post.Header.Set("Content-Type", "application/x-plist")
	reply, err := fetch(post, "sign-sap-setup-buffer")
	if err != nil {
		return err
	}
	r.Reply = hex.EncodeToString(reply)
	final, state, err := guest.Exchange(200, hardwareID, r.Context, reply)
	if err != nil {
		return err
	}
	r.Final, r.FinalState = hex.EncodeToString(final), state
	for index, input := range signInputs {
		data, _ := hex.DecodeString(input)
		signStart := time.Now()
		signature, err := guest.Sign(r.Context, data)
		if index == 0 {
			r.SignNanos = time.Since(signStart).Nanoseconds()
		}
		if err != nil {
			r.Signs = append(r.Signs, sign{Input: input, Output: "error: " + err.Error()})
			continue
		}
		r.Signs = append(r.Signs, sign{Input: input, Output: hex.EncodeToString(signature)})
	}
	if err := guest.Teardown(r.Context); err != nil {
		return err
	}
	r.FlowNanos = time.Since(start).Nanoseconds()
	if err := guest.Close(); err != nil {
		return err
	}

	kbsync, err := machine.GenerateKBSync(ctx, bundle, hardwareID, kbsyncDSID)
	if err != nil {
		r.KBSync = "error: " + err.Error()
	} else {
		r.KBSync = hex.EncodeToString(kbsync)
	}

	adi, err := machine.OpenModernADI(ctx, bundle, hostUUID, nil)
	if err != nil {
		r.ADIOTP = "error: " + err.Error()
	} else {
		mid, otp, err := adi.RequestOTP(kbsyncDSID)
		if err != nil {
			r.ADIOTP = "error: " + err.Error()
		} else {
			r.ADIOTP = hex.EncodeToString(mid) + ":" + hex.EncodeToString(otp)
		}
		_ = adi.Close()
	}

	data, err := json.MarshalIndent(r, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(output, append(data, '\n'), 0o644)
}
