//! Port of Go `profilegen/classes.go`.

use std::collections::{BTreeMap, HashMap, HashSet};

use ipatool_store::profile::Class;

/// Go's `reachableClasses`: Store implementations, referenced class identities and their
/// complete superclass chains. Imported classes without Store methods still need their
/// layouts when the bridge allocates or inspects their objects.
pub(crate) fn reachable_classes(classes: &[Class], names: &mut HashSet<String>) -> Vec<Class> {
    let mut by_name = HashMap::with_capacity(classes.len());
    for class in classes {
        by_name.insert(class.name.as_str(), class);
        if class
            .methods
            .as_ref()
            .is_some_and(|methods| !methods.is_empty())
        {
            names.insert(class.name.clone());
        }
    }
    let mut kept = BTreeMap::new();
    for name in names.iter() {
        let mut name = name.as_str();
        while !name.is_empty() && !kept.contains_key(name) {
            let Some(class) = by_name.get(name) else {
                break;
            };
            kept.insert(name, (*class).clone());
            name = &class.superclass;
        }
    }
    kept.into_values().collect()
}

#[cfg(test)]
mod tests {
    use ipatool_store::profile::Method;

    use super::*;

    fn class(name: &str, superclass: &str) -> Class {
        Class {
            name: name.to_owned(),
            superclass: superclass.to_owned(),
            ..Class::default()
        }
    }

    fn names(classes: &[Class]) -> Vec<&str> {
        classes.iter().map(|class| class.name.as_str()).collect()
    }

    #[test]
    fn keeps_referenced_layouts_store_categories_and_superclasses() {
        let mut category = class("Category", "Root");
        category.methods = Some(vec![Method {
            selector: "storeMethod".to_owned(),
            ..Method::default()
        }]);
        let mut imported = class("Imported", "Root");
        imported.size = 48;
        imported.ivars = Some(BTreeMap::from([("value".to_owned(), 16)]));
        let mut root = class("Root", "");
        root.size = 16;
        let classes = [class("Unrelated", ""), category, imported, root];
        let mut wanted = HashSet::from(["Imported".to_owned(), "Missing".to_owned()]);
        let actual = reachable_classes(&classes, &mut wanted);
        assert_eq!(names(&actual), ["Category", "Imported", "Root"]);
        assert_eq!(actual[1].size, 48);
        assert_eq!(actual[1].ivars, classes[2].ivars);
        assert_eq!(actual[2].size, 16);
    }

    #[test]
    fn terminates_cyclic_superclass_chains_deterministically() {
        let classes = [class("B", "A"), class("A", "B")];
        let actual = reachable_classes(&classes, &mut HashSet::from(["B".to_owned()]));
        assert_eq!(names(&actual), ["A", "B"]);
    }
}
