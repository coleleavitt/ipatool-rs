package fairplay

// Fixture generator for the Rust port's tests (crates/ipatool-decrypt/tests/oracle.rs).
// Synthetic credentials only. Copy this file into ipadecrypt's internal/fairplay at commit
// 7c82f86 and run:
//
//	GOTOOLCHAIN=auto go test ./internal/fairplay/ -run TestGenFixtures -count=1 \
//	    -args -out <ipatool-rs>/crates/ipatool-decrypt/tests/fixtures

import (
	"archive/zip"
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/md5"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"math/rand"
	"os"
	"path/filepath"
	"testing"

	"github.com/londek/ipadecrypt/internal/pipeline"
)

var outDir = flag.String("out", "", "fixture output directory")

type rng struct{ r *rand.Rand }

func (g rng) bytes(n int) []byte {
	b := make([]byte, n)
	g.r.Read(b)
	return b
}

func mkSinf(g rng, cred, work []byte, name string, arcade bool, privLen int) []byte {
	iviv := g.bytes(16)
	clear := g.bytes(privLen - 8*boolInt(privLen == 440))
	copy(clear[0:4], "itun")
	copy(clear[20:24], "key ")
	copy(clear[24:40], work)
	copy(clear[44:48], "iviv")
	iv := md5.Sum(append([]byte(name), iviv...))
	key := xorBytes(cred, []byte("0000000000000000"))
	block, _ := aes.NewCipher(key)
	enc := make([]byte, len(clear))
	cipher.NewCBCEncrypter(block, iv[:]).CryptBlocks(enc, clear)
	if privLen == 440 {
		enc = append(enc, make([]byte, 8)...)
	}
	keyField := []byte{3, 0, 0, 1}
	if arcade {
		keyField = []byte{4, 0, 0, 1}
	}
	nameField := append([]byte(name), 0, 0, 0)
	schi := atomOf("schi", concat(atomOf("user", []byte{0, 0, 0, 7}), atomOf("key ", keyField),
		atomOf("iviv", iviv), atomOf("name", nameField), atomOf("priv", enc)))
	return atomOf("sinf", concat(atomOf("frma", []byte("game")), atomOf("schm", make([]byte, 12)), schi))
}

func boolInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

func atomOf(tag string, body []byte) []byte {
	out := make([]byte, 8, 8+len(body))
	binary.BigEndian.PutUint32(out, uint32(8+len(body)))
	copy(out[4:], tag)
	return append(out, body...)
}

func concat(parts ...[]byte) []byte { return bytes.Join(parts, nil) }

// mkMacho: a thin little-endian Mach-O with an LC_UUID and an encryption command.
func mkMacho(g rng, cpu, sub uint32, cryptoff, pages int, cryptid uint32) []byte {
	is64 := cpu == cpuTypeARM64
	header, encCmd, encSize, magic := 28, uint32(lcEncryptionInfo), 20, uint32(machoMagicLE)
	if is64 {
		header, encCmd, encSize, magic = 32, lcEncryptionInfo64, 24, machoMagic64LE
	}
	size := cryptoff + pages*pageSize + 64
	data := make([]byte, size)
	copy(data[cryptoff:], g.bytes(pages*pageSize))
	copy(data[size-64:], g.bytes(64))
	le := binary.LittleEndian
	le.PutUint32(data[0:], magic)
	le.PutUint32(data[4:], cpu)
	le.PutUint32(data[8:], sub)
	le.PutUint32(data[12:], 2)
	le.PutUint32(data[16:], 2)
	le.PutUint32(data[20:], uint32(24+encSize))
	le.PutUint32(data[24:], 0)
	pos := header
	le.PutUint32(data[pos:], 0x1b)
	le.PutUint32(data[pos+4:], 24)
	pos += 24
	le.PutUint32(data[pos:], encCmd)
	le.PutUint32(data[pos+4:], uint32(encSize))
	le.PutUint32(data[pos+8:], uint32(cryptoff))
	le.PutUint32(data[pos+12:], uint32(pages*pageSize))
	le.PutUint32(data[pos+16:], cryptid)
	if encSize == 24 {
		le.PutUint32(data[pos+20:], 0)
	}
	return data
}

type fatSlice struct {
	cpu, sub uint32
	data     []byte
}

func mkFat(slices []fatSlice, fat64 bool) ([]byte, []int) {
	const align = 0x1000
	archSize := 20
	magic := uint32(0xcafebabe)
	if fat64 {
		archSize, magic = 32, 0xcafebabf
	}
	out := make([]byte, align)
	be := binary.BigEndian
	be.PutUint32(out[0:], magic)
	be.PutUint32(out[4:], uint32(len(slices)))
	var bases []int
	for i, s := range slices {
		for len(out)%align != 0 {
			out = append(out, 0)
		}
		base := len(out)
		bases = append(bases, base)
		off := 8 + i*archSize
		be.PutUint32(out[off:], s.cpu)
		be.PutUint32(out[off+4:], s.sub)
		if fat64 {
			be.PutUint64(out[off+8:], uint64(base))
			be.PutUint64(out[off+16:], uint64(len(s.data)))
			be.PutUint32(out[off+24:], 14)
		} else {
			be.PutUint32(out[off+8:], uint32(base))
			be.PutUint32(out[off+12:], uint32(len(s.data)))
			be.PutUint32(out[off+16:], 14)
		}
		out = append(out, s.data...)
	}
	return out, bases
}

type supfArch struct {
	cpu, sub uint32
	pages    int
}

func mkSupf(g rng, version byte, format string, archs []supfArch) []byte {
	headerSize := 12
	if version == 3 {
		headerSize = 32
	}
	out := []byte{version}
	out = append(out, format...)
	out = append(out, 0, 0, 0, 0)
	out = binary.BigEndian.AppendUint32(out, uint32(len(archs)))
	for _, a := range archs {
		length := headerSize + a.pages*32
		out = binary.BigEndian.AppendUint32(out, uint32(length))
		hdr := make([]byte, headerSize)
		binary.BigEndian.PutUint32(hdr[0:], a.cpu)
		binary.BigEndian.PutUint32(hdr[4:], a.sub)
		binary.BigEndian.PutUint32(hdr[headerSize-4:], uint32(a.pages))
		out = append(out, hdr...)
		out = append(out, g.bytes(a.pages*32)...)
	}
	return out
}

func mkSupp(g rng, version byte, totalPages int) []byte {
	out := []byte{version, '5', '0', '7'}
	if version == 1 {
		out = append(out, g.bytes(20)...)
	}
	out = binary.BigEndian.AppendUint32(out, uint32(totalPages))
	return append(out, g.bytes(totalPages*32)...)
}

type binCase struct {
	Name       string `json:"name"`
	Credential string `json:"credential"`
	Sinf       string `json:"sinf"`
	KeyFile    string `json:"key_file"`
	Source     string `json:"source"`
	Output     string `json:"output,omitempty"`
	Pages      int    `json:"pages"`
	Encrypted  bool   `json:"encrypted"`
	Error      string `json:"error,omitempty"`
	Work       string `json:"work16,omitempty"`
}

func runCase(name string, cred, sinf, keyFile, source []byte) binCase {
	c := binCase{Name: name, Credential: hex.EncodeToString(cred), Sinf: hex.EncodeToString(sinf),
		KeyFile: hex.EncodeToString(keyFile), Source: hex.EncodeToString(source), Encrypted: true}
	if w, err := deriveWork16(cred, sinf); err == nil {
		c.Work = hex.EncodeToString(w)
	}
	out, pages, err := decryptBinary(source, sinf, keyFile, cred)
	switch {
	case errors.Is(err, errNotEncrypted):
		c.Encrypted = false
	case err != nil:
		c.Error = err.Error()
	default:
		c.Output = hex.EncodeToString(out)
		c.Pages = pages
	}
	return c
}

func TestGenFixtures(t *testing.T) {
	if *outDir == "" {
		t.Skip("no -out")
	}
	g := rng{rand.New(rand.NewSource(7))}
	cred := g.bytes(16)
	arcadeCred := g.bytes(16)
	work := g.bytes(16)
	arcadeWork := g.bytes(16)
	sinf448 := mkSinf(g, cred, work, "Synthetic Name", false, 448)
	sinf440 := mkSinf(g, cred, work, "Other", false, 440)
	arcadeSinf := mkSinf(g, arcadeCred, arcadeWork, "Arcade Name", true, 448)

	var cases []binCase

	// thin arm64e, SUPF v5 612
	m := mkMacho(g, cpuTypeARM64, 0x80000002, 0x1000, 3, 1)
	cases = append(cases, runCase("thin_arm64e_supf_v5_612", cred, sinf448,
		mkSupf(g, 5, "612", []supfArch{{cpuTypeARM64, 0x80000002, 3}}), m))
	// thin arm64_32, SUPF v3 319
	m = mkMacho(g, cpuTypeARM6432, 1, 0x1000, 2, 1)
	cases = append(cases, runCase("thin_arm64_32_supf_v3_319", cred, sinf440,
		mkSupf(g, 3, "319", []supfArch{{cpuTypeARM6432, 1, 2}}), m))
	// thin armv7, SUPF v3 510
	m = mkMacho(g, cpuTypeARM, 9, 0x1000, 2, 1)
	cases = append(cases, runCase("thin_armv7_supf_v3_510", cred, sinf448,
		mkSupf(g, 3, "510", []supfArch{{cpuTypeARM, 9, 2}}), m))
	// fat armv7 + armv7s, SUPP v01: armv7 is chosen; page indexes are file-absolute.
	fat, _ := mkFat([]fatSlice{{cpuTypeARM, 11, mkMacho(g, cpuTypeARM, 11, 0x1000, 2, 1)},
		{cpuTypeARM, 9, mkMacho(g, cpuTypeARM, 9, 0x1000, 2, 1)}}, false)
	cases = append(cases, runCase("fat_armv7s_armv7_supp_v1", cred, sinf448, mkSupp(g, 1, len(fat)/pageSize), fat))
	// fat arm64 + arm64e (ipadecrypt #56): plain arm64 is chosen, SUPF v5 611 has both.
	fat, _ = mkFat([]fatSlice{{cpuTypeARM64, 0x80000002, mkMacho(g, cpuTypeARM64, 0x80000002, 0x1000, 2, 1)},
		{cpuTypeARM64, 0, mkMacho(g, cpuTypeARM64, 0, 0x1000, 3, 1)}}, false)
	cases = append(cases, runCase("fat_arm64e_arm64_supf_v5_611", cred, sinf448,
		mkSupf(g, 5, "611", []supfArch{{cpuTypeARM64, 0x80000002, 2}, {cpuTypeARM64, 0, 3}}), fat))
	// fat64 arm64_32 + arm64e: arm64e is chosen, SUPP v04.
	fat, _ = mkFat([]fatSlice{{cpuTypeARM6432, 1, mkMacho(g, cpuTypeARM6432, 1, 0x1000, 1, 1)},
		{cpuTypeARM64, 0x80000002, mkMacho(g, cpuTypeARM64, 0x80000002, 0x1000, 2, 1)}}, true)
	cases = append(cases, runCase("fat64_arm64_32_arm64e_supp_v4", cred, sinf440, mkSupp(g, 4, len(fat)/pageSize), fat))
	// fat armv7 + arm64: arm64 is chosen, SUPF v3 502 and 322.
	fat, _ = mkFat([]fatSlice{{cpuTypeARM, 9, mkMacho(g, cpuTypeARM, 9, 0x1000, 1, 1)},
		{cpuTypeARM64, 0, mkMacho(g, cpuTypeARM64, 0, 0x1000, 2, 1)}}, false)
	cases = append(cases, runCase("fat_armv7_arm64_supf_v3_502", cred, sinf448,
		mkSupf(g, 3, "502", []supfArch{{cpuTypeARM, 9, 1}, {cpuTypeARM64, 0, 2}}), fat))
	cases = append(cases, runCase("fat_armv7_arm64_supf_v3_322", cred, sinf448,
		mkSupf(g, 3, "322", []supfArch{{cpuTypeARM, 9, 1}, {cpuTypeARM64, 0, 2}}), fat))
	cases = append(cases, runCase("fat_armv7_arm64_supf_v5_507", cred, sinf448,
		mkSupf(g, 5, "507", []supfArch{{cpuTypeARM, 9, 1}, {cpuTypeARM64, 0, 2}}), fat))
	// Arcade license with the Arcade credential.
	m = mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 1)
	cases = append(cases, runCase("thin_arm64_arcade", arcadeCred, arcadeSinf,
		mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 1}}), m))
	// Errors.
	cases = append(cases, runCase("err_arcade_with_standard_credential", cred, arcadeSinf,
		mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 1}}), m))
	cases = append(cases, runCase("err_supf_unknown_format", cred, sinf448,
		mkSupf(g, 3, "999", []supfArch{{cpuTypeARM64, 0, 1}}), m))
	cases = append(cases, runCase("err_empty_supf", cred, sinf448, nil, m))
	cases = append(cases, runCase("err_supf_arch_missing", cred, sinf448,
		mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0x80000002, 1}}), m))
	cases = append(cases, runCase("err_supf_incomplete", cred, sinf448,
		mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 0}}), m))
	cases = append(cases, runCase("err_supp_page_missing", cred, sinf448, mkSupp(g, 4, 1), m))
	fat, _ = mkFat([]fatSlice{{cpuTypeARM, 9, mkMacho(g, cpuTypeARM, 9, 0x1000, 1, 1)},
		{cpuTypeARM, 9, mkMacho(g, cpuTypeARM, 9, 0x1000, 1, 1)}}, false)
	cases = append(cases, runCase("err_two_armv7", cred, sinf448, mkSupp(g, 1, len(fat)/pageSize), fat))
	fat, _ = mkFat([]fatSlice{{0x01000007, 3, mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 1)}}, false)
	cases = append(cases, runCase("err_no_arm_slice", cred, sinf448, mkSupp(g, 1, 4), fat))
	cases = append(cases, runCase("not_encrypted_cryptid_0", cred, sinf448,
		mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 1}}), mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 0)))

	writeJSON(t, "binaries.json", map[string]any{"cases": cases})

	// IPA-level fixtures.
	type member struct {
		name string
		data []byte
	}
	app := "Payload/App.app/"
	appBin, _ := mkFat([]fatSlice{{cpuTypeARM, 9, mkMacho(g, cpuTypeARM, 9, 0x1000, 1, 1)},
		{cpuTypeARM64, 0, mkMacho(g, cpuTypeARM64, 0, 0x1000, 2, 1)}}, false)
	appSinfV4 := mkSinf(g, cred, work, "App v4", false, 448)
	fw := mkMacho(g, cpuTypeARM64, 0, 0x1000, 2, 1)
	ext := mkMacho(g, cpuTypeARM64, 0x80000002, 0x1000, 1, 1)
	watch := mkMacho(g, cpuTypeARM6432, 1, 0x1000, 1, 1)
	plain := mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 0)
	members := []member{
		{"iTunesMetadata.plist", []byte("<plist/>")},
		{"Payload/", nil},
		{app, nil},
		{app + "Info.plist", []byte("<plist>app</plist>")},
		{app + "App", appBin},
		// iOS-26-only: the legacy .supf is 0 bytes, keys are in .v5.supf, SINF in .v4.sinf.
		{app + "SC_Info/App.sinf", sinf448},
		{app + "SC_Info/App.v4.sinf", appSinfV4},
		{app + "SC_Info/App.supf", nil},
		{app + "SC_Info/App.v5.supf", mkSupf(g, 5, "612", []supfArch{{cpuTypeARM, 9, 1}, {cpuTypeARM64, 0, 2}})},
		// No SINF of its own: the enclosing app's .sinf decodes it (SUPP v01).
		{app + "Frameworks/F.framework/F", fw},
		{app + "Frameworks/F.framework/SC_Info/F.supp", mkSupp(g, 1, len(fw)/pageSize)},
		{app + "Frameworks/P.framework/P", plain},
		{app + "Frameworks/P.framework/SC_Info/P.supf", mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 1}})},
		{app + "PlugIns/E.appex/E", ext},
		{app + "PlugIns/E.appex/SC_Info/E.sinf", sinf440},
		{app + "PlugIns/E.appex/SC_Info/E.supf", mkSupf(g, 3, "507", []supfArch{{cpuTypeARM64, 0x80000002, 1}})},
		{app + "PlugIns/E.appex/SC_Info/E.supp", mkSupp(g, 1, len(ext)/pageSize)},
		{app + "Watch/W.app/W", watch},
		{app + "Watch/W.app/SC_Info/W.sinf", sinf448},
		{app + "Watch/W.app/SC_Info/W.supp", mkSupp(g, 4, len(watch)/pageSize)},
		{app + "App.dSYM/Contents/x", []byte("dsym")},
		{app + "Symbols/s", []byte("sym")},
	}
	writeZip(t, "app.zip", members)

	arcadeBin := mkMacho(g, cpuTypeARM64, 0, 0x1000, 2, 1)
	writeZip(t, "arcade.zip", []member{
		{"Payload/G.app/G", arcadeBin},
		{"Payload/G.app/SC_Info/G.sinf", arcadeSinf},
		{"Payload/G.app/SC_Info/G.supf", mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 2}})},
	})

	// A framework with no key files stays encrypted: Decrypt succeeds, Verify fails.
	writeZip(t, "unplanned.zip", []member{
		{"Payload/U.app/U", mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 1)},
		{"Payload/U.app/SC_Info/U.sinf", sinf448},
		{"Payload/U.app/SC_Info/U.supf", mkSupf(g, 5, "510", []supfArch{{cpuTypeARM64, 0, 1}})},
		{"Payload/U.app/Frameworks/L.framework/L", mkMacho(g, cpuTypeARM64, 0, 0x1000, 1, 1)},
	})

	type run struct {
		Name           string            `json:"name"`
		Input          string            `json:"input"`
		Arcade         bool              `json:"arcade"`
		SkipAppex      bool              `json:"skip_appex"`
		KeepWatch      bool              `json:"keep_watch"`
		Error          string            `json:"error,omitempty"`
		Binaries       int               `json:"binaries"`
		Pages          int               `json:"pages"`
		Members        map[string]string `json:"members,omitempty"`
		StillEncrypted []string          `json:"still_encrypted"`
		Scanned        int               `json:"scanned"`
	}
	var runs []run
	for _, r := range []run{
		{Name: "app_default", Input: "app.zip"},
		{Name: "app_keep_watch", Input: "app.zip", KeepWatch: true},
		{Name: "app_skip_appex", Input: "app.zip", SkipAppex: true},
		{Name: "arcade_with_credential", Input: "arcade.zip", Arcade: true},
		{Name: "arcade_without_credential", Input: "arcade.zip"},
		{Name: "unplanned_framework", Input: "unplanned.zip"},
	} {
		creds := Credentials{Standard: cred}
		if r.Arcade {
			creds.Arcade = arcadeCred
		}
		var buf bytes.Buffer
		summary, err := Decrypt(filepath.Join(*outDir, r.Input), &buf, creds, r.SkipAppex, r.KeepWatch)
		if err != nil {
			r.Error = err.Error()
			runs = append(runs, r)
			continue
		}
		r.Binaries, r.Pages = summary.Binaries, summary.Pages
		tmp := filepath.Join(t.TempDir(), "out.ipa")
		if err := os.WriteFile(tmp, buf.Bytes(), 0o644); err != nil {
			t.Fatal(err)
		}
		res, err := pipeline.Verify(tmp, "", r.SkipAppex)
		if err != nil {
			t.Fatal(err)
		}
		r.StillEncrypted = res.StillEncrypted
		r.Scanned = res.Scanned
		zr, _ := zip.NewReader(bytes.NewReader(buf.Bytes()), int64(buf.Len()))
		r.Members = map[string]string{}
		for _, f := range zr.File {
			rc, _ := f.Open()
			var b bytes.Buffer
			b.ReadFrom(rc)
			rc.Close()
			sum := sha256.Sum256(b.Bytes())
			r.Members[f.Name] = hex.EncodeToString(sum[:])
		}
		runs = append(runs, r)
	}
	writeJSON(t, "ipas.json", map[string]any{"credential": hex.EncodeToString(cred),
		"arcade_credential": hex.EncodeToString(arcadeCred), "runs": runs})
}

func writeJSON(t *testing.T, name string, v any) {
	data, err := json.MarshalIndent(v, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(*outDir, name), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func writeZip[T interface {
	~struct {
		name string
		data []byte
	}
}](t *testing.T, name string, members []T) {
	var buf bytes.Buffer
	zw := zip.NewWriter(&buf)
	for _, m := range members {
		s := struct {
			name string
			data []byte
		}(m)
		w, err := zw.Create(s.name)
		if err != nil {
			t.Fatal(err)
		}
		w.Write(s.data)
	}
	if err := zw.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(*outDir, name), buf.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}
