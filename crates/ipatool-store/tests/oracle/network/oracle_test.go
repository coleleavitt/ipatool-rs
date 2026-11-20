package emulator

// Records crates/ipatool-store/tests/fixtures/network-oracle.json for the Rust port:
// net/url parsing and formatting, the terms URL checks, allowedURL, the query encoders and
// the Store backend's redirect rules. Run by scripts/network-oracle.sh.

import (
	"encoding/json"
	"net/url"
	"os"
	"testing"

	"github.com/majd/ipatool/v2/internal/store/terms"
)

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
