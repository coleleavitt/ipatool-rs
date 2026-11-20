package appstore

// Recorder for the Rust port (crates/ipatool-appstore/tests/fixtures/macos-package-oracle.json):
// the go-macho XAR reader, the Mac package and preflight metadata readers, the Mac offer
// selection and the package platform decision, on fixed inputs. Run by
// scripts/macos-package-oracle.sh in a temporary export.

import (
	"bytes"
	"compress/zlib"
	"crypto/md5" // #nosec G501 -- XAR checksum kind.
	"crypto/sha1" // #nosec G505 -- XAR checksum kind.
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
	"testing"

	"github.com/blacktop/go-macho/pkg/xar"
	"github.com/onsi/gomega"
)

const bzipPackageInfo = "QlpoOTFBWSZTWcBuQ48AAAifgFADqAcZAAkAP+/eQCAAVEQIMTTQaYjIbINRA9RkbUZNB6mmmguDyYIhiXFi2rJRRdIPN7ZUd8fByITUkKxML6NBQRCRzxY9BA8wh+goUlPtmwckWFxdyRThQkMBuQ48"

func oracleZlib(data []byte) []byte {
	var out bytes.Buffer
	w := zlib.NewWriter(&out)
	_, _ = w.Write(data)
	_ = w.Close()

	return out.Bytes()
}

// oracleArchive builds a XAR whose heap starts with the TOC checksum of the given kind.
func oracleArchive(kind uint32, style, files string, payload []byte) []byte {
	size := sha1.Size
	if kind == 2 {
		size = md5.Size
	}
	toc := fmt.Sprintf(`<?xml version="1.0" encoding="UTF-8"?>
<xar><toc><creation-time>2024-01-02T03:04:05</creation-time><checksum style="%s"><offset>0</offset><size>%d</size></checksum>%s</toc></xar>`, style, size, files)
	compressed := oracleZlib([]byte(toc))
	var sum []byte
	if kind == 2 {
		s := md5.Sum(compressed) // #nosec G401
		sum = s[:]
	} else {
		s := sha1.Sum(compressed) // #nosec G401
		sum = s[:]
	}
	archive := make([]byte, 28)
	binary.BigEndian.PutUint32(archive[0:4], 0x78617221)
	binary.BigEndian.PutUint16(archive[4:6], 28)
	binary.BigEndian.PutUint16(archive[6:8], 1)
	binary.BigEndian.PutUint64(archive[8:16], uint64(len(compressed)))
	binary.BigEndian.PutUint64(archive[16:24], uint64(len(toc)))
	binary.BigEndian.PutUint32(archive[24:28], kind)
	archive = append(archive, compressed...)
	archive = append(archive, sum...)

	return append(archive, payload...)
}

func oracleFile(id, name, extra, encoding string, offset int, archived []byte, style, digest string) string {
	if digest == "" {
		if style == "md5" {
			s := md5.Sum(archived) // #nosec G401
			digest = hex.EncodeToString(s[:])
		} else {
			s := sha1.Sum(archived) // #nosec G401
			digest = hex.EncodeToString(s[:])
		}
	}

	return fmt.Sprintf(`<file id="%s"><type>file</type><name>%s</name>%s<data><length>%d</length><offset>%d</offset><size>%d</size><encoding style="%s"/><archived-checksum style="%s">%s</archived-checksum><extracted-checksum style="%s">%s</extracted-checksum></data></file>`,
		id, name, extra, len(archived), offset, len(archived), encoding, style, digest, style, digest)
}

func oracleXARCases() [][2]interface{} {
	octet := "application/octet-stream"
	gzip := "application/x-gzip"
	info := []byte(`<pkg-info><bundle id="com.example.mac" CFBundleShortVersionString="1.2.1"/></pkg-info>`)
	payload := []byte("app payload")
	gz := oracleZlib(info)
	bz, _ := base64.StdEncoding.DecodeString(bzipPackageInfo)
	cases := [][2]interface{}{}
	add := func(name string, data []byte) { cases = append(cases, [2]interface{}{name, data}) }

	add("plain file", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "sha1", ""), payload))
	add("md5 archive", oracleArchive(2, "md5", oracleFile("1", "Payload", "", octet, 16, payload, "md5", ""), payload))
	add("md5 header sha1 toc", oracleArchive(2, "sha1", "", nil))
	add("sha1 header md5 toc", oracleArchive(1, "md5", "", nil))
	add("no checksum kind", oracleArchive(0, "sha1", "", nil))
	add("unknown checksum kind", oracleArchive(7, "sha1", "", nil))
	add("empty toc", oracleArchive(1, "sha1", "", nil))
	add("gzip entry", oracleArchive(1, "sha1", oracleFile("1", "PackageInfo", "", gzip, 20, gz, "sha1", ""), gz))
	add("bzip2 entry", oracleArchive(1, "sha1", oracleFile("1", "PackageInfo", "", "application/x-bzip2", 20, bz, "sha1", ""), bz))
	add("corrupt gzip entry", oracleArchive(1, "sha1", oracleFile("1", "PackageInfo", "", gzip, 20, info, "sha1", ""), info))
	add("unsupported encoding", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", "application/x-xz", 20, payload, "sha1", ""), payload))
	add("wrong file checksum", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "sha1", strings.Repeat("ff", 20)), payload))
	add("md5 file checksum", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "MD5", ""), payload))
	add("truncated heap", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "sha1", ""), payload[:4]))
	add("offset past heap", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 4000, payload, "sha1", ""), payload))
	add("nested directories", oracleArchive(1, "sha1",
		`<file id="1"><type>directory</type><name>com.example.pkg</name>`+
			oracleFile("2", "PackageInfo", "<mtime>2025-07-24T08:38:42Z</mtime><mode>0644</mode><uid>501</uid><gid> 20 </gid><user>cole</user>", octet, 20, info, "sha1", "")+
			oracleFile("3", "Payload", "<mtime>2025-07-24T08:38:42+02:00</mtime><ctime>2025-07-24T08:38:42.5Z</ctime>", octet, 20+len(info), payload, "sha1", "")+
			`<file id="4"><type>directory</type><name>../Resources</name>`+oracleFile("5", "en.lproj", "", octet, 20, info, "sha1", "")+`</file></file>`+
			`<file id="9"><type>symlink</type><name>link</name></file><file id="x"><type>fifo</type></file>`,
		append(append([]byte(nil), info...), payload...)))
	add("duplicate ids", oracleArchive(1, "sha1", oracleFile("1", "A", "", octet, 20, payload, "sha1", "")+oracleFile("1", "B", "", octet, 20, payload, "sha1", ""), payload))
	add("repeated name element", oracleArchive(1, "sha1", oracleFile("1", "A", "<name>B</name>", octet, 20, payload, "sha1", ""), payload))
	add("uppercase checksum style", oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "SHA1", ""), payload))
	add("missing data", oracleArchive(1, "sha1", `<file id="1"><type>file</type><name>x</name></file>`, nil))
	add("directory with data errors ignored", oracleArchive(1, "sha1", `<file id="1"><type>directory</type><name>d</name><data><archived-checksum style="crc">zz</archived-checksum></data></file>`, nil))
	add("bad id", oracleArchive(1, "sha1", oracleFile("-1", "x", "", octet, 20, payload, "sha1", ""), payload))
	add("plus id", oracleArchive(1, "sha1", oracleFile("+1", "x", "", octet, 20, payload, "sha1", ""), payload))
	add("bad hex", oracleArchive(1, "sha1", oracleFile("1", "x", "", octet, 20, payload, "sha1", "zz"), payload))
	add("odd hex", oracleArchive(1, "sha1", oracleFile("1", "x", "", octet, 20, payload, "sha1", "abc"), payload))
	add("odd hex with bad last byte", oracleArchive(1, "sha1", oracleFile("1", "x", "", octet, 20, payload, "sha1", "abz"), payload))
	add("unknown file checksum style", oracleArchive(1, "sha1", oracleFile("1", "x", "", octet, 20, payload, "crc32", ""), payload))
	add("bad uid", oracleArchive(1, "sha1", oracleFile("1", "x", "<uid>x</uid>", octet, 20, payload, "sha1", ""), payload))
	add("blank uid", oracleArchive(1, "sha1", oracleFile("1", "x", "<uid>  </uid>", octet, 20, payload, "sha1", ""), payload))
	add("empty uid", oracleArchive(1, "sha1", oracleFile("1", "x", "<uid></uid>", octet, 20, payload, "sha1", ""), payload))
	add("bad mtime", oracleArchive(1, "sha1", oracleFile("1", "x", "<mtime>2025-07-24 08:38:42</mtime>", octet, 20, payload, "sha1", ""), payload))
	add("bad data length", oracleArchive(1, "sha1", `<file id="1"><type>file</type><name>x</name><data><length>1x</length></data></file>`, nil))

	plain := oracleArchive(1, "sha1", oracleFile("1", "Payload", "", octet, 20, payload, "sha1", ""), payload)
	add("not xar", []byte("not xar"))
	add("empty", []byte{})
	bad := append([]byte(nil), plain...)
	bad[0] = 0
	add("bad magic", bad)
	bad = append([]byte(nil), plain...)
	bad[7] = 2
	add("bad version", bad)
	bad = append([]byte(nil), plain...)
	bad[5] = 29
	add("bad header size", bad)
	bad = append([]byte(nil), plain...)
	bad[len(bad)-len(payload)-1] ^= 1
	add("toc checksum mismatch", bad)
	add("short toc checksum", plain[:len(plain)-len(payload)-3])
	add("toc length past end", plain[:40])
	bad = append([]byte(nil), plain...)
	bad[28] ^= 0xff
	add("corrupt zlib", bad)
	add("other root element", oracleRawTOC(`<archive><toc/></archive>`))
	add("malformed toc", oracleRawTOC(`<xar><toc>`))

	return cases
}

func oracleRawTOC(toc string) []byte {
	compressed := oracleZlib([]byte(toc))
	archive := make([]byte, 28)
	binary.BigEndian.PutUint32(archive[0:4], 0x78617221)
	binary.BigEndian.PutUint16(archive[4:6], 28)
	binary.BigEndian.PutUint16(archive[6:8], 1)
	binary.BigEndian.PutUint64(archive[8:16], uint64(len(compressed)))
	binary.BigEndian.PutUint64(archive[16:24], uint64(len(toc)))
	binary.BigEndian.PutUint32(archive[24:28], 1)
	archive = append(archive, compressed...)
	s := sha1.Sum(compressed) // #nosec G401

	return append(archive, s[:]...)
}

type oracleXARFile struct {
	ID        uint64 `json:"id"`
	Name      string `json:"name"`
	Mtime     int64  `json:"mtime"`
	Mode      uint32 `json:"mode"`
	UID       int    `json:"uid"`
	GID       int    `json:"gid"`
	Verify    bool   `json:"verify"`
	Content   []byte `json:"content,omitempty"`
	OpenError string `json:"openError,omitempty"`
	ReadError string `json:"readError,omitempty"`
}

type oracleXARResult struct {
	Name  string          `json:"name"`
	Input []byte          `json:"input"`
	Error string          `json:"error,omitempty"`
	Files []oracleXARFile `json:"files"`
}

type oracleMetadataResult struct {
	Kind           string `json:"kind"`
	Name           string `json:"name"`
	Input          []byte `json:"input"`
	Bundle         string `json:"bundle"`
	Error          string `json:"error,omitempty"`
	DisplayVersion string `json:"displayVersion,omitempty"`
	ReleaseDate    int64  `json:"releaseDate,omitempty"`
}

type oracleOfferResult struct {
	Name     string `json:"name"`
	Body     string `json:"body"`
	AppID    int64  `json:"appId"`
	BundleID string `json:"bundleId"`
	Version  string `json:"version,omitempty"`
	Error    string `json:"error,omitempty"`
}

type oracleSinf struct {
	Data   []byte `json:"data"`
	DPInfo []byte `json:"dpInfo"`
}

type oraclePlatformResult struct {
	Platform string            `json:"platform"`
	Metadata map[string]string `json:"metadata"`
	Sinfs    []oracleSinf      `json:"sinfs"`
	Result   string            `json:"result,omitempty"`
	Error    string            `json:"error,omitempty"`
}

func oracleErr(err error) string {
	if err == nil {
		return ""
	}

	return err.Error()
}

func TestRustOracle(t *testing.T) {
	// The package's XAR fixture helpers assert with Gomega.
	gomega.RegisterTestingT(t)
	out := map[string]interface{}{}

	var xars []oracleXARResult
	for _, c := range oracleXARCases() {
		name, data := c[0].(string), c[1].([]byte)
		result := oracleXARResult{Name: name, Input: data, Files: []oracleXARFile{}}
		archive, err := xar.NewReader(bytes.NewReader(data), int64(len(data)))
		if err != nil {
			result.Error = err.Error()
		} else {
			ids := make([]uint64, 0, len(archive.Files))
			for id := range archive.Files {
				ids = append(ids, id)
			}
			sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
			for _, id := range ids {
				file := archive.Files[id]
				entry := oracleXARFile{ID: id, Name: file.Name, Mtime: file.Info.Mtime, Mode: file.Info.Mode, UID: file.Info.Uid, GID: file.Info.Gid, Verify: file.VerifyChecksum()}
				reader, err := file.Open()
				if err != nil {
					entry.OpenError = err.Error()
				} else {
					content, err := io.ReadAll(reader)
					_ = reader.Close()
					entry.ReadError = oracleErr(err)
					if err == nil {
						entry.Content = content
					}
				}
				result.Files = append(result.Files, entry)
			}
		}
		xars = append(xars, result)
	}
	out["xar"] = xars

	var metadata []oracleMetadataResult
	good := macPreflightDistribution("4.2", "4.2.0.0.1.1341568473")
	preflights := [][3]string{
		{"good", good, ""},
		{"good with bundle", good, "com.example.mac"},
		{"wrong bundle", good, "com.other.mac"},
		{"missing version", macPreflightDistribution("", "4.2.0.0.1.1341568473"), ""},
		{"blank version", macPreflightDistribution("  ", "4.2.0.0.1.1341568473"), ""},
		{"missing timestamp", macPreflightDistribution("4.2", ""), ""},
		{"dotted", macPreflightDistribution("4.2", "4.2.0"), ""},
		{"different", macPreflightDistribution("4.2", "4.3.0.0.1.1341568473"), ""},
		{"letters", macPreflightDistribution("4.2", "4.2.0.0.1.abcdefghij"), ""},
		{"zero", macPreflightDistribution("4.2", "4.2.0.0.1.0000000000"), ""},
		{"three part display", macPreflightDistribution("4.2.1", "4.2.1.0.1.1341568473"), ""},
		{"four part display", macPreflightDistribution("4.2.1.3", "4.2.1.0.1.1341568473"), ""},
		{"one part display", macPreflightDistribution("4", "4.0.0.0.1.1341568473"), ""},
		{"signed timestamp", macPreflightDistribution("4.2", "4.2.0.0.1.+341568473"), ""},
		{"unrelated", strings.ReplaceAll(good, `<bundle id="com.example.mac"`, `<bundle id="com.other.mac"`), ""},
		{"conflicting", strings.Replace(good, "1341568473", "1341568474", 1), ""},
		{"ambiguous", strings.Replace(good, "</installer-gui-script>", `<product id="com.other.mac" version="4.2"/></installer-gui-script>`, 1), ""},
		{"no product id", strings.Replace(good, `product id="com.example.mac"`, `product`, 1), ""},
		{"malformed", "<", ""},
		{"other root", "<pkg-info/>", ""},
		{"prolog and comments", `<?xml version="1.0" encoding="utf-8" standalone="no"?><!-- c -->` + good, ""},
		{"nested product ignored", strings.Replace(good, "</installer-gui-script>", `<choices><product id="com.other.mac" version="4.2"/></choices></installer-gui-script>`, 1), ""},
	}
	for _, c := range preflights {
		data := macPreflightXAR(c[1])
		archive, err := xar.NewReader(bytes.NewReader(data), int64(len(data)))
		if err != nil {
			t.Fatal(err)
		}
		m, err := macPreflightVersionMetadata(archive, c[2])
		r := oracleMetadataResult{Kind: "preflight", Name: c[0], Input: data, Bundle: c[2], Error: oracleErr(err)}
		if err == nil {
			r.DisplayVersion, r.ReleaseDate = m.DisplayVersion, m.ReleaseDate.Unix()
		}
		metadata = append(metadata, r)
	}
	info := `<pkg-info><bundle id="com.example.mac" CFBundleShortVersionString="1.2.1"/></pkg-info>`
	packages := [][4]string{
		{"matching", info, "com.example.mac", "2025-07-24T08:38:42Z"},
		{"id only", info, "", "2025-07-24T08:38:42Z"},
		{"wrong bundle", info, "com.other.mac", "2025-07-24T08:38:42Z"},
		{"missing version", `<pkg-info><bundle id="com.example.mac"/></pkg-info>`, "com.example.mac", "2025-07-24T08:38:42Z"},
		{"missing timestamp", info, "com.example.mac", ""},
		{"epoch timestamp", info, "com.example.mac", "1970-01-01T00:00:00Z"},
		{"ambiguous", `<pkg-info><bundle id="a" CFBundleShortVersionString="1"/><bundle id="b" CFBundleShortVersionString="2"/></pkg-info>`, "", "2025-07-24T08:38:42Z"},
		{"filtered ambiguous", `<pkg-info><bundle id="a" CFBundleShortVersionString="1"/><bundle id="b" CFBundleShortVersionString="2"/></pkg-info>`, "b", "2025-07-24T08:38:42Z"},
		{"nested bundles ignored", `<pkg-info><payload><bundle id="com.example.mac" CFBundleShortVersionString="9"/></payload>` + strings.TrimPrefix(info, "<pkg-info>"), "", "2025-07-24T08:38:42Z"},
		{"invalid", "<", "com.example.mac", "2025-07-24T08:38:42Z"},
	}
	for _, c := range packages {
		data := macMetadataXAR(c[1], c[3])
		archive, err := xar.NewReader(bytes.NewReader(data), int64(len(data)))
		r := oracleMetadataResult{Kind: "package", Name: c[0], Input: data, Bundle: c[2]}
		if err != nil {
			r.Error = "xar: " + err.Error()
		} else {
			m, err := macPackageVersionMetadata(archive, c[2])
			r.Error = oracleErr(err)
			if err == nil {
				r.DisplayVersion, r.ReleaseDate = m.DisplayVersion, m.ReleaseDate.Unix()
			}
		}
		metadata = append(metadata, r)
	}
	// A product archive: PackageInfo and Payload under a component package directory.
	component := oracleArchive(1, "sha1",
		`<file id="1"><type>directory</type><name>com.example.pkg</name>`+
			oracleFile("2", "PackageInfo", "", "application/x-gzip", 20, oracleZlib([]byte(info)), "sha1", "")+
			oracleFile("3", "Payload", "<mtime>2025-07-24T08:38:42Z</mtime>", "application/octet-stream", 20, []byte(info), "sha1", "")+`</file>`+
			oracleFile("4", "Payload", "<mtime>2001-01-01T00:00:00Z</mtime>", "application/octet-stream", 20, []byte(info), "sha1", ""),
		append(oracleZlib([]byte(info)), []byte(info)...))
	archive, err := xar.NewReader(bytes.NewReader(component), int64(len(component)))
	if err != nil {
		t.Fatal(err)
	}
	m, err := macPackageVersionMetadata(archive, "")
	r := oracleMetadataResult{Kind: "package", Name: "component directory", Input: component, Error: oracleErr(err)}
	if err == nil {
		r.DisplayVersion, r.ReleaseDate = m.DisplayVersion, m.ReleaseDate.Unix()
	}
	metadata = append(metadata, r)
	out["metadata"] = metadata

	var offers []oracleOfferResult
	mac := `{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac","phone"],"buyParams":"salableAdamId=6472431552&appExtVrsId=876660716"}}`
	bodies := [][2]string{
		{"mac", `<script id="serialized-server-data">` + mac + `</script>`},
		{"single quotes", `<html><script type="application/json" id='serialized-server-data'>  [` + mac + `,` + mac + `] </script>`},
		{"nested", `<script id="serialized-server-data">{"data":[{"x":{"y":[` + mac + `]}}]}</script>`},
		{"conflict", `<script id="serialized-server-data">[` + mac + `,` + strings.Replace(mac, "876660716", "1", 1) + `]</script>`},
		{"escaped params", `<script id="serialized-server-data">` + strings.Replace(mac, "appExtVrsId=876660716", "app%45xtVrsId=876660716&x=a+b", 1) + `</script>`},
		{"semicolon params", `<script id="serialized-server-data">` + strings.Replace(mac, "&appExt", ";x=1&appExt", 1) + `</script>`},
		{"bad escape", `<script id="serialized-server-data">` + strings.Replace(mac, "&appExt", "&x=%zz&appExt", 1) + `</script>`},
		{"repeated key first wins", `<script id="serialized-server-data">` + strings.Replace(mac, "appExtVrsId=876660716", "appExtVrsId=5&appExtVrsId=876660716", 1) + `</script>`},
		{"zero version", `<script id="serialized-server-data">` + strings.Replace(mac, "876660716", "0", 1) + `</script>`},
		{"signed version", `<script id="serialized-server-data">` + strings.Replace(mac, "876660716", "+876660716", 1) + `</script>`},
		{"huge version", `<script id="serialized-server-data">` + strings.Replace(mac, "876660716", "99999999999999999999", 1) + `</script>`},
		{"numeric platforms", `<script id="serialized-server-data">` + strings.Replace(mac, `["mac","phone"]`, `[1,"mac"]`, 1) + `</script>`},
		{"missing", `<html></html>`},
		{"no script", `<div id="serialized-server-data">{}</div>`},
		{"malformed", `<script id="serialized-server-data"`},
		{"not closed", `<script id="serialized-server-data">{}`},
		{"invalid json", `<script id="serialized-server-data">{</script>`},
		{"empty", `<script id="serialized-server-data"></script>`},
	}
	apps := []App{{ID: 6472431552, BundleID: "com.nebula.karing"}, {ID: 6472431552}, {ID: 6472431552, BundleID: "other"}, {ID: 1}}
	for _, body := range bodies {
		for _, app := range apps {
			version, err := macOSExternalVersionID([]byte(body[1]), app)
			offers = append(offers, oracleOfferResult{Name: body[0], Body: body[1], AppID: app.ID, BundleID: app.BundleID, Version: version, Error: oracleErr(err)})
		}
	}
	out["offers"] = offers

	var platforms []oraclePlatformResult
	metas := []map[string]string{
		{}, {"software-platform": "macos"}, {"software-platform": "MacOS"}, {"software-platform": "ios"}, {"software-platform": "IOS", "product-type": "mac-os-app"},
		{"product-type": "ios-app"}, {"product-type": "mac-os-app"}, {"software-platform": "macos", "product-type": "ios-app"}, {"software-platform": "watchos"},
	}
	sinfSets := [][]oracleSinf{
		nil,
		{{Data: []byte("sinf")}},
		{{DPInfo: []byte("dp")}},
		{{Data: []byte("sinf"), DPInfo: []byte("dp")}},
		{{Data: []byte("sinf")}, {DPInfo: []byte("dp")}},
		{{Data: []byte("sinf")}, {}},
		{{}},
	}
	for _, platform := range []Platform{PlatformMacOS, PlatformIPhone, ""} {
		for _, meta := range metas {
			for _, sinfs := range sinfSets {
				item := downloadItemResult{Metadata: map[string]interface{}{}}
				for k, v := range meta {
					item.Metadata[k] = v
				}
				for _, s := range sinfs {
					item.Sinfs = append(item.Sinfs, Sinf{Data: s.Data, DPInfo: s.DPInfo})
				}
				result, err := downloadPackagePlatform(platform, item)
				platforms = append(platforms, oraclePlatformResult{Platform: string(platform), Metadata: meta, Sinfs: sinfs, Result: string(result), Error: oracleErr(err)})
			}
		}
	}
	out["platforms"] = platforms

	data, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("IPATOOL_ORACLE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
