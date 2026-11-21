//! Port of Go `profilegen/objc.go`: Objective-C class layouts and Store method entry points.

use std::collections::{BTreeMap, HashSet};

use ipatool_store::profile::{Class, Method};

use crate::error::{Error, Result};
use crate::source::{STORE_IMAGES, Source, inside, relative_to};

impl Source<'_> {
    /// Go's `objcClass`: the class at `address` and its superclass chain, memoized. Returns
    /// the class address, or `None` for a null pointer.
    pub(crate) fn objc_class(&mut self, address: u64) -> Result<Option<u64>> {
        if address == 0 {
            return Ok(None);
        }
        if self.classes.contains_key(&address) {
            return Ok(Some(address));
        }
        let data = self.u64(address + 32)? & !7;
        let name = class_name(&self.cstring(self.u64(data + 24)?)?);
        let class = Class {
            address,
            meta_address: self.u64(address)?,
            ivars: Some(BTreeMap::new()),
            size: (u64::from(self.u32(data + 8)?) + 7) & !7,
            name,
            ..Class::default()
        };
        if class.name.is_empty() {
            return Err(Error::panic(format!(
                "class at {address:#x} has no name (ro {data:#x})"
            )));
        }
        let meta_address = class.meta_address;
        self.classes.insert(address, class);

        let superclass = self.u64(address + 8)?;
        if let Some(superclass) = self.objc_class(superclass)? {
            let name = self.classes[&superclass].name.clone();
            self.class_mut(address).superclass = name;
        }
        let mut methods = self.methods(self.u64(data + 32)?, false)?;
        let meta_data = self.u64(meta_address + 32)? & !7;
        methods.extend(self.methods(self.u64(meta_data + 32)?, true)?);
        append_methods(self.class_mut(address), methods);

        let list = self.u64(data + 48)?;
        if list != 0 {
            let (stride, count) = (self.u32(list)?, self.u32(list + 4)?);
            if stride < 32 || count > 1 << 16 {
                return Err(Error::panic("invalid ivar list"));
            }
            for index in 0..u64::from(count) {
                let entry = list + 8 + index * u64::from(stride);
                let name = self.cstring(self.u64(entry + 8)?)?;
                let offset = self.u64(entry)?;
                if offset != 0 {
                    let value = u64::from(self.u32(offset)?);
                    ivars(self.class_mut(address)).insert(name, value);
                }
            }
        }
        Ok(Some(address))
    }

    fn class_mut(&mut self, address: u64) -> &mut Class {
        self.classes.entry(address).or_default()
    }

    /// Go's `methods`: a method list, a relative-list-of-lists, or small methods.
    fn methods(&self, mut list: u64, meta: bool) -> Result<Vec<Method>> {
        if list == 0 {
            return Ok(Vec::new());
        }
        if list & 1 != 0 {
            list &= !1;
            let count = self.u32(list + 4)?;
            if count > 1 << 16 {
                return Err(Error::panic("invalid attached method lists"));
            }
            let mut result = Vec::new();
            for index in 0..u64::from(count) {
                let entry = list + 8 + index * 8;
                let offset = self.u64(entry)?.cast_signed() >> 16;
                result.extend(self.methods(entry.wrapping_add_signed(offset), meta)?);
            }
            return Ok(result);
        }

        let (flags, count) = (self.u32(list)?, self.u32(list + 4)?);
        if count == 0 {
            return Ok(Vec::new());
        }
        let stride = u64::from(flags & 0xfffc);
        if stride < 12 || count > 1 << 18 {
            return Err(Error::panic(format!(
                "invalid method list {list:#x} ({flags:x},{count})"
            )));
        }
        let mut result = Vec::new();
        for index in 0..u64::from(count) {
            let entry = list + 8 + index * stride;
            let (selector, types, implementation) = if flags & 0x8000_0000 != 0 {
                let selector = if flags & 0x4000_0000 != 0 {
                    self.selector_base + u64::from(self.u32(entry)?)
                } else {
                    self.u64(relative_to(entry, self.u32(entry)?))?
                };
                let types = if flags & 0x2000_0000 != 0 {
                    self.selector_base + u64::from(self.u32(entry + 4)?)
                } else {
                    relative_to(entry + 4, self.u32(entry + 4)?)
                };
                (
                    selector,
                    types,
                    relative_to(entry + 8, self.u32(entry + 8)?),
                )
            } else {
                (
                    self.u64(entry)?,
                    self.u64(entry + 8)?,
                    self.u64(entry + 16)?,
                )
            };
            if inside(implementation, &self.target) {
                result.push(Method {
                    selector: self.cstring(selector)?,
                    types: self.cstring(types)?,
                    address: implementation,
                    class_method: meta,
                });
            }
        }
        Ok(result)
    }

    /// Go's `objcClasses`: every class in the closure plus Store categories, deduplicated and
    /// sorted.
    pub(crate) fn objc_classes(&mut self) -> Result<Vec<Class>> {
        let lists: Vec<(u64, u64)> = self
            .images
            .values()
            .flat_map(|image| &image.sections)
            .filter(|section| section.name == "__objc_classlist")
            .map(|section| (section.address, section.size))
            .collect();
        for (address, size) in lists {
            let mut offset = 0;
            while offset < size {
                let class = self.u64(address + offset)?;
                self.objc_class(class)?;
                offset += 8;
            }
        }
        // Categories can install Store methods on Foundation classes.
        let categories: Vec<(u64, u64)> = STORE_IMAGES
            .iter()
            .flat_map(|name| &self.images[*name].sections)
            .filter(|section| section.name == "__objc_catlist")
            .map(|section| (section.address, section.size))
            .collect();
        for (address, size) in categories {
            let mut offset = 0;
            while offset < size {
                let category = self.u64(address + offset)?;
                let class = self
                    .objc_class(self.u64(category + 8)?)?
                    .ok_or_else(|| Error::panic("unresolved category class"))?;
                let mut methods = self.methods(self.u64(category + 16)?, false)?;
                methods.extend(self.methods(self.u64(category + 24)?, true)?);
                append_methods(self.class_mut(class), methods);
                offset += 8;
            }
        }

        let mut classes: Vec<Class> = self
            .classes
            .values_mut()
            .map(|class| {
                if let Some(methods) = &mut class.methods {
                    let mut seen = HashSet::new();
                    methods.retain(|method| {
                        seen.insert((method.class_method, method.selector.clone()))
                    });
                    methods.sort_by(|a, b| {
                        a.class_method
                            .cmp(&b.class_method)
                            .then_with(|| a.selector.cmp(&b.selector))
                    });
                }
                class.clone()
            })
            .collect();
        classes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(classes)
    }
}

/// Go's `append(c.Methods, ...)`: the list stays nil until a method is added.
pub(crate) fn append_methods(class: &mut Class, methods: Vec<Method>) {
    if !methods.is_empty() {
        class.methods.get_or_insert_with(Vec::new).extend(methods);
    }
}

pub(crate) fn ivars(class: &mut Class) -> &mut BTreeMap<String, u64> {
    class.ivars.get_or_insert_with(BTreeMap::new)
}

/// Go's `className`: the runtime's qualified form of public Swift class names. Private and
/// generic encodings stay mangled.
pub(crate) fn class_name(name: &str) -> String {
    let Some(mut value) = name.strip_prefix("_TtC") else {
        return name.to_owned();
    };
    let mut parts = Vec::new();
    if let Some(rest) = value.strip_prefix('s') {
        parts.push("Swift");
        value = rest;
    }
    while !value.is_empty() {
        let digits = value.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return name.to_owned();
        }
        let Ok(length) = value[..digits].parse::<usize>() else {
            return name.to_owned();
        };
        if length > value.len() - digits {
            return name.to_owned();
        }
        let Some(part) = value.get(digits..digits + length) else {
            return name.to_owned();
        };
        parts.push(part);
        value = &value[digits + length..];
    }
    if parts.len() < 2 {
        return name.to_owned();
    }
    parts.join(".")
}
