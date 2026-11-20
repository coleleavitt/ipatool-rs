package emulator

// Records crates/ipatool-store/tests/fixtures/network-oracle.json for the Rust port:
// net/url parsing and formatting, the terms URL checks, allowedURL, the query encoders and
// the Store backend's redirect rules. Run by scripts/network-oracle.sh.

import (
	"bytes"
	"context"
	"encoding/hex"
	"io"
	"encoding/json"
	"math"
	"net/http"
	"time"
	"net/url"
	"os"
	"testing"

	"github.com/majd/ipatool/v2/internal/sap/assets"
	"github.com/majd/ipatool/v2/internal/store/profile"
	"github.com/majd/ipatool/v2/internal/store/terms"
	"howett.net/plist"
)

// typed describes a host value so the Rust side can rebuild it exactly.
func typed(value any) any {
	switch v := value.(type) {
	case nil:
		return map[string]any{"t": "nil"}
	case string:
		return map[string]any{"t": "string", "v": v}
	case int64:
		return map[string]any{"t": "int", "v": v}
	case float64:
		return map[string]any{"t": "real", "v": math.Float64bits(v)}
	case bool:
		return map[string]any{"t": "bool", "v": v}
	case []byte:
		return map[string]any{"t": "data", "v": hex.EncodeToString(v)}
	case time.Time:
		return map[string]any{"t": "date", "s": v.Unix(), "n": v.Nanosecond()}
	case []any:
		items := []any{}
		for _, item := range v {
			items = append(items, typed(item))
		}
		return map[string]any{"t": "array", "v": items}
	case map[string]any:
		entries := map[string]any{}
		for key, item := range v {
			entries[key] = typed(item)
		}
		return map[string]any{"t": "dict", "v": entries}
	case uint64:
		return map[string]any{"t": "int", "v": int64(v)}
	default:
		return map[string]any{"t": "other"}
	}
}

type codecCase struct {
	Value                  any
	XML, Binary, JSON      string
	XMLError, BinaryError  string
	JSONError              string
}

func oracleCodecValues() []any {
	date := time.Unix(1795204679, 123456789).UTC()
	return []any{
		map[string]any{"a": "x", "b": int64(-5), "c": 1.5, "d": true, "e": false},
		map[string]any{"empty": "", "data": []byte{}, "nested": map[string]any{}, "list": []any{}},
		map[string]any{"text": "<&>\"'\t\n\r\x01\u2028 é 😀", "nil": nil, "key with space": "v"},
		map[string]any{"floats": []any{1e6, 123456.0, 1234567.0, 1e-5, 0.0001, 0.1, 1e21, 1e20, -0.0, 0.0, 3.0, 1e-7, 2.5e-300, math.MaxFloat64}},
		map[string]any{"ints": []any{int64(0), int64(255), int64(256), int64(65535), int64(65536), int64(4294967295), int64(4294967296), int64(-1), int64(math.MinInt64), int64(math.MaxInt64)}},
		map[string]any{"date": date, "data": []byte("hello world, this is sixteen+"), "same": []any{"dup", "dup", true, true, false, int64(7), int64(7), 7.0, []byte("x"), []byte("x")}},
		[]any{"root", "array", nil, map[string]any{"k": "v"}},
		"plain string root",
		map[string]any{"long": string(make([]byte, 20)), "unicode": "ünïcödé string longer than fifteen"},
		map[string]any{"nan": math.NaN()},
		map[string]any{"inf": math.Inf(1), "ninf": math.Inf(-1)},
		nil,
	}
}

func oracleCodec() []codecCase {
	var cases []codecCase
	for _, value := range oracleCodecValues() {
		c := codecCase{Value: typed(value)}
		if data, err := plist.Marshal(value, plist.XMLFormat); err != nil {
			c.XMLError = err.Error()
		} else {
			c.XML = string(data)
		}
		if data, err := func() (data []byte, err error) {
			defer func() {
				if r := recover(); r != nil {
					err = errorString("panic")
				}
			}()
			return plist.Marshal(value, plist.BinaryFormat)
		}(); err != nil {
			c.BinaryError = err.Error()
		} else {
			c.Binary = hex.EncodeToString(data)
		}
		if data, err := json.Marshal(value); err != nil {
			c.JSONError = err.Error()
		} else {
			c.JSON = string(data)
		}
		cases = append(cases, c)
	}
	return cases
}

type errorString string

func (e errorString) Error() string { return string(e) }

type decodeCase struct {
	Input, Format string
	Value         any
	Error         bool
}

func oracleDecode() []decodeCase {
	inputs := []struct{ format, input string }{
		{"json", `{"a":1,"b":[1.5,true,null,"x"],"c":{"d":-2e3},"big":12345678901234567890}`},
		{"json", `[]`},
		{"json", `"\u003c\ud83d\ude00"`},
		{"json", `{"a":1,}`},
		{"json", `{"a":1} x`},
		{"plist", `<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>i</key><integer>42</integer><key>n</key><integer>-3</integer><key>r</key><real>2.5</real><key>t</key><true/><key>d</key><data>aGk=</data><key>dt</key><date>2026-11-20T19:57:59Z</date><key>a</key><array><string>x</string></array></dict></plist>`},
		{"plist", `<plist version="1.0"><string>bare</string></plist>`},
		{"plist", `not a plist`},
	}
	var cases []decodeCase
	for _, in := range inputs {
		c := decodeCase{Input: in.input, Format: in.format}
		var value any
		var err error
		if in.format == "json" {
			err = json.Unmarshal([]byte(in.input), &value)
		} else {
			_, err = plist.Unmarshal([]byte(in.input), &value)
		}
		if err != nil {
			c.Error = true
		} else {
			c.Value = typed(value)
		}
		cases = append(cases, c)
	}
	if data, err := plist.Marshal(map[string]any{"k": "v", "n": int64(-1), "list": []any{1.5, true}}, plist.BinaryFormat); err == nil {
		var value any
		_, err = plist.Unmarshal(data, &value)
		cases = append(cases, decodeCase{Input: hex.EncodeToString(data), Format: "bplist-hex", Value: typed(value), Error: err != nil})
	}
	return cases
}

var oracleURLs = []string{
	"https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://P45-BUY.itunes.apple.com:443/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"HTTPS://p7-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage?x=1",
	"https://p45-buy.itunes.apple.com:8443/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://p-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://p4x-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://q45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://user@buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage#top",
	"https://buy.itunes.apple.com/WebObjects/%4DZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://buy.itunes.apple.com/x/../WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"http://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://buy.itunes.apple.com\\WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
	"https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct",
	"https://init.itunes.apple.com/bag.xml?guid=000102030405",
	"https://iosapps.itunes.apple.com/itunes-assets/a.ipa?accessKey=1_2",
	"http://iosapps.itunes.apple.com/itunes-assets/a.ipa",
	"https://is1-ssl.mzstatic.com/image/thumb/a.jpg",
	"https://updates.cdn-apple.com/x.ipsw",
	"https://apple.com/",
	"https://evilapple.com/",
	"https://apple.com.evil.net/",
	"https://cdn.example.net/signed?sig=a%2Fb",
	"ftp://apple.com/",
	"https:///path",
	"https://[::1]:443/x",
	"https://[fe80::1%25en0]/x",
	"https://[1.2.3.4]/x",
	"https://a:b:c@apple.com/",
	"https://a%zz@apple.com/",
	"https://host:port/",
	"https://a:1:2/",
	"https://%41pple.com/",
	"https://%C3%A9.apple.com/",
	"https://example.com/a b?c d#e f",
	"https://example.com/a%2Fb?x=%zz#f%20g",
	"https://example.com/a?",
	"https://example.com/a??",
	"https://example.com/#",
	"mailto:someone@example.com",
	"/relative/path?q=1",
	"relative/path",
	"a:b/c",
	"./a:b",
	"//host/path",
	"///triple",
	"*",
	"",
	"https://example.com/\x7f",
	"https://example.com/é?é#é",
	"https://ex%ample.com/",
	"http://[::1",
	"http://x[y]/",
	"https://example.com/%",
	"https://example.com:/x",
	"https://Example.COM/A",
}

type urlFields struct {
	Error                                         string `json:"error,omitempty"`
	Scheme, Opaque, Host, Path, RawPath, Fragment string
	RawQuery, RawFragment                         string
	User                                          *string
	ForceQuery, OmitHost                          bool
	String, EscapedPath, Hostname, Port           string
	IsPageURL, IsBuyHost                          bool
	AllowedAuthenticated, AllowedAnonymous        bool
}

func oracleFields(raw string) urlFields {
	u, err := url.Parse(raw)
	if err != nil {
		return urlFields{Error: err.Error(), IsPageURL: terms.IsPageURL(raw)}
	}
	fields := urlFields{
		Scheme: u.Scheme, Opaque: u.Opaque, Host: u.Host, Path: u.Path, RawPath: u.RawPath, Fragment: u.Fragment,
		RawQuery: u.RawQuery, RawFragment: u.RawFragment, ForceQuery: u.ForceQuery, OmitHost: u.OmitHost,
		String: u.String(), EscapedPath: u.EscapedPath(), Hostname: u.Hostname(), Port: u.Port(),
		IsPageURL: terms.IsPageURL(raw), IsBuyHost: terms.IsBuyHost(u.Hostname()),
		AllowedAuthenticated: allowedURL(u, true), AllowedAnonymous: allowedURL(u, false),
	}
	if u.User != nil {
		user := u.User.String()
		fields.User = &user
	}

	return fields
}

type resolveCase struct{ Base, Reference, Result, Error string }

type joinCase struct {
	Base           string
	Elements       []string
	Result, Error  string
}

type escapeCase struct {
	Input                                     string
	QueryEscape, ComponentQuery              string
	QueryUnescape, PathUnescape              string
	QueryUnescapeError, PathUnescapeError    string
}

type cookieCase struct {
	Line                                string
	OK                                  bool
	Name, Value, Path, Domain           string
	Quoted, Secure, HTTPOnly, HasExpiry bool
	ExpiresUnix                         int64
	ExpiresNanos                        int
	AddCookie                           string
}

var oracleCookies = []string{
	"session=fixture; Path=/",
	"a=b; Domain=.apple.com; Path=/WebObjects; Secure; HttpOnly",
	"mz_at0=AAA; expires=Fri, 20-Nov-2026 19:57:59 GMT; path=/; domain=.apple.com",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 GMT",
	"x=1; Expires=fri, 20 nov 2026 9:57:59 GMT",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 GMT+3",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 GMT-23",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 GMT+24",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 +03",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 UTC",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 PDT",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 CEST",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 ABCD",
	"x=1; Expires=Fri, 31 Nov 2026 19:57:59 GMT",
	"x=1; Expires=Fri, 29 Feb 2028 19:57:59 GMT",
	"x=1; Expires=Fri, 29 Feb 2027 19:57:59 GMT",
	"x=1; Expires=Xyz, 20 Nov 2026 19:57:59 GMT",
	"x=1; Expires=Fri, 20 Nov 2026 24:00:00 GMT",
	"x=1; Expires=Fri, 20 Nov 2026 19:57:59 GMT; Expires=bogus",
	"x=1; Expires=Fri, 2 Nov 2026 19:57:59 GMT",
	"x=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
	"\"quoted\"=1",
	"q=\"a b\"; Path=\"/x\"",
	"q=\"\"",
	"q=\"",
	"bad name=1",
	"novalue",
	"",
	" ; ",
	"=value",
	"n=v\\x",
	"n=v,w",
	"n= spaced ; secure",
	"n=v; Domain=bad\"domain; Path=/ok",
	"n=v;;;Secure;HTTPONLY",
	"n=v; Max-Age=0; SameSite=Lax; Partitioned",
}

type capturedRequest struct {
	Method, URL string
	Headers     map[string][]string
	Body        string
}

type executeCase struct {
	Name                  string
	Requests              []capturedRequest
	Status                int
	Headers               map[string][]string
	Body, Error           string
}

func oracleExecute(t *testing.T) []executeCase {
	ctx := context.Background()
	bundle, err := assets.Load(ctx)
	if err != nil {
		t.Fatal(err)
	}
	prepared, err := profile.Load(ctx)
	if err != nil {
		t.Fatal(err)
	}
	plistBody := []byte(`<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>status</key><integer>0</integer><key>items</key><array><string>a</string><real>1.5</real></array><key>empty</key><string></string></dict></plist>`)
	jsonBody := []byte(`{"results":[{"trackId":1,"name":"x <y>"}],"count":1.5}`)
	type spec struct {
		name     string
		in       Request
		body     []byte
		ct       string
		redirect string
	}
	specs := []spec{
		{"plain GET", Request{Operation: "request", Method: http.MethodGet, URL: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/ping?a=1&b=two", Headers: http.Header{"X-Dsid": {"12345"}, "X-Token": {"token"}, "X-Apple-Store-Front": {"143441-1,29"}, "User-Agent": {"oracle"}}}, plistBody, "text/xml", ""},
		{"encoded POST", Request{Operation: "request", Method: http.MethodPost, URL: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases", Parameters: map[string]any{"guid": "000102030405", "count": int64(3), "flag": true, "nested": map[string]any{"k": "v"}}, Headers: http.Header{"User-Agent": {"oracle"}}}, plistBody, "text/xml; charset=UTF-8", ""},
		{"json response", Request{Operation: "request", Method: http.MethodGet, URL: "https://itunes.apple.com/lookup?id=1", Format: "json", Headers: http.Header{"User-Agent": {"oracle"}}}, jsonBody, "text/javascript; charset=utf-8", ""},
		{"raw body PUT", Request{Operation: "request", Method: http.MethodPut, URL: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/raw", Body: []byte("raw-bytes"), Raw: true, Headers: http.Header{"User-Agent": {"oracle"}, "Content-Type": {"application/octet-stream"}, "Content-Length": {"9"}}}, []byte("opaque"), "application/octet-stream", ""},
		{"purchase", Request{Operation: "purchase", Method: http.MethodPost, URL: "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct", Bag: map[string]any{"buyProduct": "https://buy.itunes.apple.com/request"}, Parameters: map[string]any{"price": "0", "productType": "C", "salableAdamId": "1234", "pricingParameters": "STDQ", "guid": "000102030405"}, Headers: http.Header{"User-Agent": {"oracle"}, "X-Dsid": {"12345"}, "X-Token": {"token"}}}, plistBody, "text/xml", ""},
		{"paid purchase", Request{Operation: "purchase", Method: http.MethodPost, URL: "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct", Bag: map[string]any{"buyProduct": "https://buy.itunes.apple.com/request"}, Parameters: map[string]any{"price": "990", "productType": "C"}}, plistBody, "text/xml", ""},
		{"disallowed host", Request{Operation: "request", Method: http.MethodGet, URL: "https://example.com/x"}, plistBody, "text/xml", ""},
		{"unsupported method", Request{Operation: "request", Method: http.MethodDelete, URL: "https://p25-buy.itunes.apple.com/x", Parameters: map[string]any{}}, plistBody, "text/xml", ""},
		{"redirect to another pod", Request{Operation: "request", Method: http.MethodPost, URL: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases", Parameters: map[string]any{"a": "b"}, Headers: http.Header{"User-Agent": {"oracle"}, "X-Dsid": {"12345"}, "X-Token": {"token"}, "Cookie": {"c=d"}}}, plistBody, "text/xml", "https://p26-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases?x=1"},
		{"redirect without following", Request{Operation: "request", Method: http.MethodGet, URL: "https://p25-buy.itunes.apple.com/a", NoRedirects: true, Headers: http.Header{"User-Agent": {"oracle"}}}, plistBody, "text/xml", "/b"},
		{"redirect to a foreign host", Request{Operation: "request", Method: http.MethodGet, URL: "https://p25-buy.itunes.apple.com/a", Headers: http.Header{"User-Agent": {"oracle"}}}, plistBody, "text/xml", "https://example.com/b"},
		{"stream transfer", Request{Operation: "transfer", Method: http.MethodGet, URL: "http://downloads.example.net/file.ipa", Stream: true, Headers: http.Header{"User-Agent": {"oracle"}}}, []byte("PK-package-bytes"), "application/octet-stream", ""},
		{"authenticate", Request{Operation: "authenticate", Method: http.MethodPost, URL: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate", Parameters: map[string]any{"appleId": "user@example.com", "password": "secret", "attempt": "4", "why": "signIn", "guid": "000102030405"}, Headers: http.Header{"User-Agent": {"oracle"}}, Sign: func(data []byte) ([]byte, error) { return []byte{1, 2, 3, byte(len(data) % 256)}, nil }}, plistBody, "text/xml", ""},
	}
	var cases []executeCase
	for _, s := range specs {
		guest, err := open(ctx, bundle.AppleMediaServices, prepared, true)
		if err != nil {
			t.Fatal(err)
		}
		c := executeCase{Name: s.name}
		in := s.in
		in.HardwareID = []byte{0, 1, 2, 3, 4, 5}
		body, ct, redirect := s.body, s.ct, s.redirect
		in.Transport = func(request *http.Request) (*http.Response, error) {
			data, _ := io.ReadAll(request.Body)
			c.Requests = append(c.Requests, capturedRequest{Method: request.Method, URL: request.URL.String(), Headers: request.Header, Body: string(data)})
			if redirect != "" && len(c.Requests) == 1 {
				return &http.Response{StatusCode: http.StatusFound, Header: http.Header{"Location": {redirect}, "Set-Cookie": {"r=1; Path=/"}}, Body: io.NopCloser(bytes.NewReader(nil)), Request: request}, nil
			}

			return &http.Response{StatusCode: http.StatusOK, Header: http.Header{"Content-Type": {ct}, "Set-Cookie": {"a=b; Path=/"}}, Body: io.NopCloser(bytes.NewReader(body)), ContentLength: int64(len(body))}, nil
		}
		response, err := guest.Execute(in)
		if err != nil {
			c.Error = err.Error()
		} else {
			data, _ := io.ReadAll(response.Body)
			c.Status, c.Headers, c.Body = response.StatusCode, response.Header, string(data)
		}
		_ = guest.Close()
		cases = append(cases, c)
	}

	return cases
}

func TestRustOracle(t *testing.T) {
	out := os.Getenv("IPATOOL_ORACLE_OUT")
	if out == "" {
		t.Skip("IPATOOL_ORACLE_OUT is not set")
	}
	record := map[string]any{}
	urls := map[string]urlFields{}
	for _, raw := range oracleURLs {
		urls[raw] = oracleFields(raw)
	}
	record["urls"] = urls
	hosts := map[string]bool{}
	for _, host := range []string{"buy.itunes.apple.com", "BUY.ITUNES.APPLE.COM", "p1-buy.itunes.apple.com", "p-buy.itunes.apple.com", "p12a-buy.itunes.apple.com", "pp1-buy.itunes.apple.com", "p١-buy.itunes.apple.com", "x.buy.itunes.apple.com", "", "-buy.itunes.apple.com", "p007-buy.itunes.apple.com"} {
		hosts[host] = terms.IsBuyHost(host)
	}
	record["buy_hosts"] = hosts

	var cookies []cookieCase
	for _, line := range oracleCookies {
		c := cookieCase{Line: line}
		if cookie, err := http.ParseSetCookie(line); err == nil {
			c.OK = true
			c.Name, c.Value, c.Path, c.Domain = cookie.Name, cookie.Value, cookie.Path, cookie.Domain
			c.Quoted, c.Secure, c.HTTPOnly = cookie.Quoted, cookie.Secure, cookie.HttpOnly
			if !cookie.Expires.IsZero() {
				c.HasExpiry, c.ExpiresUnix, c.ExpiresNanos = true, cookie.Expires.Unix(), cookie.Expires.Nanosecond()
			}
			request := &http.Request{Header: http.Header{}}
			request.AddCookie(&http.Cookie{Name: "first", Value: "1"})
			request.AddCookie(cookie)
			c.AddCookie = request.Header.Get("Cookie")
		}
		cookies = append(cookies, c)
	}
	record["cookies"] = cookies
	record["codec"] = oracleCodec()
	record["decode"] = oracleDecode()
	record["execute"] = oracleExecute(t)

	var resolves []resolveCase
	for _, pair := range [][2]string{
		{"https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate", "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate"},
		{"https://buy.itunes.apple.com/a/b/c?x=1#f", "/d/e"},
		{"https://buy.itunes.apple.com/a/b/c?x=1#f", "d/../e"},
		{"https://buy.itunes.apple.com/a/b/c?x=1#f", ""},
		{"https://buy.itunes.apple.com/a/b/c?x=1#f", "?y=2"},
		{"https://buy.itunes.apple.com/a/b/c?x=1#f", "#g"},
		{"https://buy.itunes.apple.com/a/b/c", "//other.apple.com/x/./y"},
		{"https://buy.itunes.apple.com/a/b/", "../../../.."},
		{"https://buy.itunes.apple.com/a/b/", "."},
		{"https://buy.itunes.apple.com/a", "x%2Fy"},
		{"https://buy.itunes.apple.com/a", "http://h:x/"},
		{"mailto:a@b", ""},
		{"mailto:a@b", "c"},
	} {
		base, _ := url.Parse(pair[0])
		ref, err := url.Parse(pair[1])
		if err != nil {
			resolves = append(resolves, resolveCase{Base: pair[0], Reference: pair[1], Error: err.Error()})

			continue
		}
		resolves = append(resolves, resolveCase{Base: pair[0], Reference: pair[1], Result: base.ResolveReference(ref).String()})
	}
	record["resolve"] = resolves

	var joins []joinCase
	for _, c := range []joinCase{
		{Base: "https://bag.itunes.apple.com/a", Elements: []string{"b"}},
		{Base: "https://bag.itunes.apple.com/a/", Elements: []string{"b/"}},
		{Base: "https://bag.itunes.apple.com", Elements: []string{"../x", "./y"}},
		{Base: "https://bag.itunes.apple.com/a%2Fb", Elements: []string{"c d"}},
		{Base: "rel/a", Elements: []string{"../../b"}},
		{Base: "https://h/a", Elements: []string{"%zz"}},
		{Base: "https://h/a", Elements: []string{}},
		{Base: "https://h/a", Elements: []string{""}},
		{Base: "https://h/a?q=1#f", Elements: []string{"b"}},
		{Base: "%", Elements: []string{"b"}},
	} {
		result, err := url.JoinPath(c.Base, c.Elements...)
		c.Result = result
		if err != nil {
			c.Error = err.Error()
		}
		joins = append(joins, c)
	}
	record["join"] = joins

	var escapes []escapeCase
	for _, input := range []string{"", "plain", "a b+c", "a%20b", "%zz", "%4", "a/b?c=d&e#f", "é~!*'();:@&=+$,/?#[]", "100%", "%2B+", "\x00\x7f"} {
		c := escapeCase{Input: input, QueryEscape: url.QueryEscape(input), ComponentQuery: encodeComponentQuery(input)}
		var err error
		if c.QueryUnescape, err = url.QueryUnescape(input); err != nil {
			c.QueryUnescapeError = err.Error()
		}
		if c.PathUnescape, err = url.PathUnescape(input); err != nil {
			c.PathUnescapeError = err.Error()
		}
		escapes = append(escapes, c)
	}
	record["escape"] = escapes

	data, err := json.MarshalIndent(record, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
