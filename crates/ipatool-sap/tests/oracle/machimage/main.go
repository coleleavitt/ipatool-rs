// Command machimage-oracle records what Go internal/sap/machimage writes into guest
// memory for the pinned SAP images, for the Rust port's oracle test.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/majd/ipatool/v2/internal/sap/machimage"
)

type write struct {
	Address uint64 `json:"address"`
	Size    int    `json:"size"`
	SHA256  string `json:"sha256"`
}

type export struct {
	Name    string `json:"name"`
	Address uint64 `json:"address,omitempty"`
	Error   string `json:"error,omitempty"`
}

type record struct {
	Name              string      `json:"name"`
	File              string      `json:"file"`
	Shared            bool        `json:"shared"`
	Base              uint64      `json:"base"`
	LoadBase          uint64      `json:"load_base,omitempty"`
	KernelBase        uint64      `json:"kernel_base,omitempty"`
	Prelinked         bool        `json:"prelinked,omitempty"`
	Exports           []export    `json:"exports"`
	SymbolCount       int         `json:"symbol_count"`
	SymbolsSHA256     string      `json:"symbols_sha256"`
	InstructionRanges [][2]uint64 `json:"instruction_ranges"`
	Resolved          []string    `json:"resolved"`
	Maps              [][2]uint64 `json:"maps"`
	Writes            []write     `json:"writes"`
}

type memory struct{ r *record }

func (m memory) MemMap(address, size uint64) error {
	m.r.Maps = append(m.r.Maps, [2]uint64{address, size})
	return nil
}

func (m memory) MemWrite(address uint64, data []byte) error {
	sum := sha256.Sum256(data)
	m.r.Writes = append(m.r.Writes, write{address, len(data), hex.EncodeToString(sum[:])})
	return nil
}

func resolveAddress(name string) uint64 {
	h := fnv.New64a()
	h.Write([]byte(name))
	return h.Sum64() & 0x00007ffffffffff0
}

type spec struct {
	name, file       string
	shared           bool
	base, kernelBase uint64
	prelinked        bool
	exports          []string
}

func main() {
	dir, output := os.Args[1], os.Args[2]
	specs := []spec{
		{name: "CoreFP", file: "CoreFP", shared: true, base: 0x0000100000000000, exports: []string{"_WIn9UJ86JKdV4dM", "_X46O5IeS", "_YlCJ3lg", "_dku592fbFAj", "_fdjkDSAFjklaf2s", "_lxpgvVMLd0S7uRl"}},
		{name: "AppleMediaServices", file: "AppleMediaServices", shared: true, prelinked: true, exports: []string{"_cp2g1b9ro", "_Mib5yocT", "_Fc3vhtJDvr", "_IPaI1oem5iL", "_jEHf8Xzsv8K"}},
		{name: "commerce", file: "commerce", shared: true, base: 0x00001000c0000000},
		{name: "adid", file: "adid", shared: true, base: 0x0000200000000000},
		{name: "fairplayd", file: "fairplayd", shared: true, base: 0x0000600000000000},
		{name: "FairPlayIOKit", file: "FairPlayIOKit", shared: true, base: 0x0000700000000510, kernelBase: 0xfffffe0007004000},
		{name: "CoreFP-unshared", file: "CoreFP", base: 0x0000100000000000},
	}
	var records []record
	for _, s := range specs {
		input, err := os.ReadFile(filepath.Join(dir, s.file))
		if err != nil {
			panic(err)
		}
		var image *machimage.Image
		if s.shared {
			image, err = machimage.OpenShared(s.name, input)
		} else {
			image, err = machimage.Open(s.name, input)
		}
		if err != nil {
			panic(err)
		}
		r := record{Name: s.name, File: s.file, Shared: s.shared, Base: image.Base(), KernelBase: s.kernelBase, Prelinked: s.prelinked}
		names := append([]string{}, s.exports...)
		symbols := sha256.New()
		picked := 0
		for _, symbol := range image.Symbols() {
			fmt.Fprintf(symbols, "%s\x00%s\x00%d\x00%d\x00%d\x00%d\n", symbol.Name, symbol.IndirectName, symbol.Type, symbol.Sect, symbol.Desc, symbol.Value)
			if picked < 8 && symbol.Sect != 0 && symbol.Type&0x01 != 0 && symbol.Name != "" {
				names = append(names, symbol.Name)
				picked++
			}
		}
		r.SymbolCount = len(image.Symbols())
		r.SymbolsSHA256 = hex.EncodeToString(symbols.Sum(nil))
		if picked > 0 {
			names = append(names, strings.ToUpper(names[len(names)-1]))
		}
		names = append(names, "_ipatool_oracle_missing")
		loadBase := s.base
		if s.prelinked {
			loadBase = image.Base()
		}
		r.LoadBase = loadBase
		for _, name := range names {
			address, err := image.Export(name, loadBase)
			if err != nil {
				r.Exports = append(r.Exports, export{Name: name, Error: err.Error()})
			} else {
				r.Exports = append(r.Exports, export{Name: name, Address: address})
			}
		}
		r.InstructionRanges = image.InstructionRanges()
		resolved := map[string]bool{}
		resolve := func(name string) (uint64, error) {
			resolved[name] = true
			return resolveAddress(name), nil
		}
		if s.prelinked {
			err = image.PreparePrelinked(prelinked, resolve)
		} else {
			err = image.Relocate(s.base, resolve)
		}
		if err != nil {
			panic(err)
		}
		if s.kernelBase != 0 {
			if err := image.RelocateKernelCachePointers(s.base, s.kernelBase, resolve); err != nil {
				panic(err)
			}
		}
		if err := image.Load(memory{&r}); err != nil {
			panic(err)
		}
		for name := range resolved {
			r.Resolved = append(r.Resolved, name)
		}
		sort.Strings(r.Resolved)
		records = append(records, r)
	}
	type prelinkedEntry struct {
		Address uint64 `json:"address"`
		Name    string `json:"name"`
	}
	var entries []prelinkedEntry
	for address, name := range prelinked {
		entries = append(entries, prelinkedEntry{address, name})
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Address < entries[j].Address })
	data, err := json.MarshalIndent(map[string]any{"prelinked": entries, "images": records}, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(output, append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}

// prelinked is Go internal/sap/machine macOS27SharedCacheFunctions.
var prelinked = map[uint64]string{
	0x0000000180675ce8: "_malloc",
	0x00000001806766f0: "_free",
	0x00000001806cf570: "_dispatch_once",
	0x000000018071acc8: "_sysctlbyname",
	0x000000018071bec4: "_gettimeofday",
	0x0000000180721fb8: "_arc4random",
	0x00000001807238a4: "___memset_chk",
	0x00000001807903c4: "_abort",
	0x0000000180849604: "___error",
	0x0000000180889bc4: "_pthread_once",
	0x000000018088a6f8: "_pthread_rwlock_unlock",
	0x000000018088a7c0: "_pthread_rwlock_wrlock",
	0x000000018088b2d8: "_pthread_rwlock_init",
	0x0000000180894b40: "_strlen",
	0x00000001808cf540: "_CFStringCreateWithCString",
	0x00000001808d4200: "_CFStringGetLength",
	0x00000001808d44dc: "_CFStringGetCString",
	0x00000001808d7278: "_CFRelease",
	0x00000001808e89e8: "_CFDataGetLength",
	0x0000000184d6b4cc: "_IOServiceMatching",
	0x0000000184d6c168: "_IOServiceGetMatchingServices",
	0x0000000184d6c4fc: "_IOIteratorNext",
	0x0000000184d6cb50: "_IOObjectRelease",
}
