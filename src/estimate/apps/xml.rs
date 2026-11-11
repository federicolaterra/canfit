//! A tiny XML tree for the settings files of virtual machines (UTM `config.plist`, Parallels
//! `config.pvs`, VirtualBox `.vbox`). Built on `quick-xml`; only elements, attributes and text.

use quick_xml::Reader;
use quick_xml::events::Event;

/// One XML element with its attributes and children.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    /// Child elements, in order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// The first child element called `name`.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name == name)
    }

    /// The first element reached by following `path` from here.
    pub fn find(&self, path: &[&str]) -> Option<&Element> {
        path.iter().try_fold(self, |e, name| e.child(name))
    }

    /// The element's text, trimmed.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for n in &self.children {
            if let Node::Text(t) = n {
                out.push_str(t);
            }
        }
        out.trim().to_string()
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parses `text` and returns its root element.
pub fn parse(text: &str) -> Result<Element, String> {
    let mut reader = Reader::from_str(text);
    // The stack's first entry collects the document's top-level nodes.
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("malformed XML near byte {}: {e}", reader.buffer_position()))?;
        match event {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let e = element(&start)?;
                push(&mut stack, Node::Element(e));
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err("malformed XML: unexpected closing tag".to_string());
                }
                let done = stack.pop().expect("checked above");
                push(&mut stack, Node::Element(done));
            }
            Event::Text(t) => push(&mut stack, Node::Text(t.xml10_content().into_owned())),
            Event::CData(t) => push(&mut stack, Node::Text(t.into_inner().into_owned())),
            Event::GeneralRef(r) => {
                let name = r.xml10_content();
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => quick_xml::escape::resolve_predefined_entity(&name)
                        .unwrap_or("")
                        .to_string(),
                };
                push(&mut stack, Node::Text(resolved));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err("malformed XML: the document ends inside an element".to_string());
    }
    let top = stack.pop().expect("one entry");
    top.children
        .into_iter()
        .find_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
        .ok_or_else(|| "the file holds no XML element".to_string())
}

fn push(stack: &mut [Element], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.children.push(node);
    }
}

fn element(start: &quick_xml::events::BytesStart<'_>) -> Result<Element, String> {
    let name = start.name().as_ref().to_string();
    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.map_err(|e| format!("malformed XML attribute in <{name}>: {e}"))?;
        let key = attr.key.as_ref().to_string();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| format!("malformed XML attribute in <{name}>: {e}"))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(Element {
        name,
        attrs,
        children: Vec::new(),
    })
}

/// A value of an XML property list.
#[derive(Debug, Clone, PartialEq)]
pub enum Plist {
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    String(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    /// `<data>`, `<date>` and anything else canfit does not need.
    Other,
}

impl Plist {
    /// Parses an XML property list (`<plist><dict>…</dict></plist>`). Binary property lists are
    /// not supported: the caller reports them as unreadable.
    pub fn parse(text: &str) -> Result<Plist, String> {
        if text.starts_with("bplist") {
            return Err("binary property lists are not supported".to_string());
        }
        let root = parse(text)?;
        if root.name != "plist" {
            return Err(format!(
                "expected a <plist> document, found <{}>",
                root.name
            ));
        }
        let value = root
            .elements()
            .next()
            .ok_or_else(|| "the property list is empty".to_string())?;
        Ok(Plist::from_element(value))
    }

    fn from_element(e: &Element) -> Plist {
        match e.name.as_str() {
            "dict" => {
                let mut entries = Vec::new();
                let mut key: Option<String> = None;
                for child in e.elements() {
                    if child.name == "key" {
                        key = Some(child.text());
                    } else if let Some(k) = key.take() {
                        entries.push((k, Plist::from_element(child)));
                    }
                }
                Plist::Dict(entries)
            }
            "array" => Plist::Array(e.elements().map(Plist::from_element).collect()),
            "string" => Plist::String(e.text()),
            "integer" => e.text().parse().map_or(Plist::Other, Plist::Integer),
            "real" => e.text().parse().map_or(Plist::Other, Plist::Real),
            "true" => Plist::Bool(true),
            "false" => Plist::Bool(false),
            _ => Plist::Other,
        }
    }

    /// The value at `key` of a dictionary.
    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Follows a path of dictionary keys.
    pub fn path(&self, keys: &[&str]) -> Option<&Plist> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Plist::Integer(i) => Some(*i),
            Plist::Real(r) if r.fract() == 0.0 => Some(*r as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Plist::String(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_attributes_and_text() {
        let root = parse(
            r#"<?xml version="1.0"?>
<!-- comment -->
<VirtualBox version="1.19">
  <Machine name="Debian &amp; friends" uuid="{1}">
    <Hardware><Memory RAMSize="2048"/></Hardware>
    <Description><![CDATA[a <b> c]]></Description>
  </Machine>
</VirtualBox>"#,
        )
        .unwrap();
        assert_eq!(root.name, "VirtualBox");
        let machine = root.child("Machine").unwrap();
        assert_eq!(machine.attr("name"), Some("Debian & friends"));
        assert_eq!(
            root.find(&["Machine", "Hardware", "Memory"])
                .and_then(|m| m.attr("RAMSize")),
            Some("2048")
        );
        assert_eq!(machine.child("Description").unwrap().text(), "a <b> c");
    }

    #[test]
    fn malformed_xml_is_an_error() {
        for bad in ["", "<a><b></a>", "<a>", "not xml", "<a x=1/>"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn property_lists() {
        let p = Plist::parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>ConfigurationVersion</key><integer>4</integer>
  <key>Information</key><dict><key>Name</key><string>Ubuntu</string></dict>
  <key>System</key><dict><key>MemorySize</key><integer>8192</integer><key>Hypervisor</key><true/></dict>
  <key>Drive</key><array><dict/><dict/></array>
</dict></plist>"#,
        )
        .unwrap();
        assert_eq!(
            p.get("ConfigurationVersion").and_then(Plist::as_i64),
            Some(4)
        );
        assert_eq!(
            p.path(&["Information", "Name"]).and_then(Plist::as_str),
            Some("Ubuntu")
        );
        assert_eq!(
            p.path(&["System", "MemorySize"]).and_then(Plist::as_i64),
            Some(8192)
        );
        assert_eq!(p.path(&["System", "Hypervisor"]), Some(&Plist::Bool(true)));
        assert!(matches!(p.get("Drive"), Some(Plist::Array(a)) if a.len() == 2));
        assert!(Plist::parse("bplist00…").is_err());
        assert!(Plist::parse("<dict/>").is_err());
    }
}
