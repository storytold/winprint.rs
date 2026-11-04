use super::{NS_PSF, NS_XSI};
use std::{borrow::Cow, io::Write};
use xml::{
    attribute::Attribute, name::OwnedName, namespace::Namespace, writer::XmlEvent, EmitterConfig,
    EventWriter,
};

// mark document root
impl XmlDocumentRoot for super::PrintCapabilitiesDocument {}
impl XmlDocumentRoot for super::PrintTicketDocument {}

/// A trait for serializing as XML element.
pub trait XmlSerializer {
    /// Collect the namespace URIs and prefixes used in the XML element.
    fn collect_namespace(&self, ns: &mut Namespace);
    /// Write the XML element to the writer.
    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error>;
}

/// A trait for serializing as XML document.
pub trait XmlDocumentRoot: XmlSerializer {
    /// Convert the struct to XML.
    fn to_xml(&self) -> Vec<u8> {
        let mut writer = EmitterConfig::new()
            .perform_indent(false)
            .create_writer(Vec::new());
        let mut ns = Namespace::empty();
        self.collect_namespace(&mut ns);
        self.write_to(&mut writer).unwrap();
        writer.into_inner()
    }
}

impl XmlSerializer for super::PrintCapabilitiesDocument {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
        self.parameter_defs
            .iter()
            .for_each(|x| x.collect_namespace(ns));
        self.features.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        {
            let mut ns = Namespace::empty();
            self.collect_namespace(&mut ns);
            let event = XmlEvent::StartElement {
                name: "psf:PrintCapabilities".into(),
                attributes: Cow::Owned(vec![Attribute::new("version".into(), "1")]),
                namespace: Cow::Owned(ns),
            };
            writer.write(event)?;
        }
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.parameter_defs
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.features.iter().try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::PrintTicketDocument {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
        self.parameter_inits
            .iter()
            .for_each(|x| x.collect_namespace(ns));
        self.features.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        {
            let mut ns = Namespace::empty();
            self.collect_namespace(&mut ns);
            let event = XmlEvent::StartElement {
                name: "psf:PrintTicket".into(),
                attributes: Cow::Owned(vec![Attribute::new("version".into(), "1")]),
                namespace: Cow::Owned(ns),
            };
            writer.write(event)?;
        }
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.parameter_inits
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.features.iter().try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::PrintFeature {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        collect_namespace_from_name(&self.name, ns);
        self.options.iter().for_each(|x| x.collect_namespace(ns));
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
        self.features.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        writer.write(
            XmlEvent::start_element("psf:Feature").attr("name", format_name(&self.name).as_ref()),
        )?;
        self.options.iter().try_for_each(|x| x.write_to(writer))?;
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.features.iter().try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::ParameterInit {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        collect_namespace_from_name(&self.name, ns);
        self.value.collect_namespace(ns);
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        writer.write(
            XmlEvent::start_element("psf:ParameterInit")
                .attr("name", format_name(&self.name).as_ref()),
        )?;
        self.value.write_to(writer)?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::ParameterDef {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        collect_namespace_from_name(&self.name, ns);
        for x in &self.properties {
            x.collect_namespace(ns);
        }
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        writer.write(
            XmlEvent::start_element("psf:ParameterDef")
                .attr("name", format_name(&self.name).as_ref()),
        )?;
        for x in &self.properties {
            x.write_to(writer)?;
        }
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::PrintFeatureOption {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        if let Some(name) = &self.name {
            collect_namespace_from_name(name, ns);
        }
        self.scored_properties
            .iter()
            .for_each(|x| x.collect_namespace(ns));
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        start_element_with_optional_name(writer, "psf:Option", self.name.as_ref())?;
        self.scored_properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::ScoredProperty {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        if let Some(name) = &self.name {
            collect_namespace_from_name(name, ns);
        }
        if let Some(parameter_ref) = &self.parameter_ref {
            collect_namespace_from_name(parameter_ref, ns);
        }
        if let Some(value) = &self.value {
            value.collect_namespace(ns);
        }
        self.scored_properties
            .iter()
            .for_each(|x| x.collect_namespace(ns));
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        start_element_with_optional_name(writer, "psf:ScoredProperty", self.name.as_ref())?;
        if let Some(parameter_ref) = &self.parameter_ref {
            writer.write(
                XmlEvent::start_element("psf:ParameterRef")
                    .attr("name", format_name(parameter_ref).as_ref()),
            )?;
            writer.write(XmlEvent::end_element())?;
        }
        if let Some(value) = &self.value {
            value.write_to(writer)?;
        }
        self.scored_properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::Property {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        collect_namespace_from_name(&self.name, ns);
        if let Some(value) = &self.value {
            value.collect_namespace(ns);
        }
        self.properties.iter().for_each(|x| x.collect_namespace(ns));
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        writer.write(
            XmlEvent::start_element("psf:Property").attr("name", format_name(&self.name).as_ref()),
        )?;
        if let Some(value) = &self.value {
            value.write_to(writer)?;
        }
        self.properties
            .iter()
            .try_for_each(|x| x.write_to(writer))?;
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

impl XmlSerializer for super::PropertyValue {
    fn collect_namespace(&self, ns: &mut Namespace) {
        ns.put("psf", NS_PSF);
        ns.put("xsi", NS_XSI);
        collect_namespace_from_name(&self.xsi_type(), ns);
        if let super::PropertyValue::QName(q) = self {
            collect_namespace_from_name(q, ns);
        }
    }

    fn write_to<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<(), xml::writer::Error> {
        let xsi_type = self.xsi_type();
        writer.write(
            XmlEvent::start_element("psf:Value").attr("xsi:type", format_name(&xsi_type).as_ref()),
        )?;
        match self {
            super::PropertyValue::String(s) => {
                writer.write(XmlEvent::characters(s))?;
            }
            super::PropertyValue::Integer(i) => {
                writer.write(XmlEvent::characters(&i.to_string()))?;
            }
            // `OwnedName`'s `Display` prefixes the namespace URI (`{uri}psk:Name`), which is not
            // a valid QName, so drivers reject the ticket.
            super::PropertyValue::QName(q) => {
                writer.write(XmlEvent::characters(format_name(q).as_ref()))?;
            }
            super::PropertyValue::Unknown(_, s) => {
                writer.write(XmlEvent::characters(s))?;
            }
        }
        writer.write(XmlEvent::end_element())?;
        Ok(())
    }
}

fn collect_namespace_from_name(name: &OwnedName, ns: &mut Namespace) {
    if let (Some(prefix), Some(namespace)) = (name.prefix_ref(), name.namespace_ref()) {
        ns.put(prefix, namespace);
    }
}

/// Format the name as a string, with the prefix if present. But no namespace URI.
fn format_name(name: &OwnedName) -> Cow<'_, String> {
    if let Some(prefix) = name.prefix_ref() {
        Cow::Owned(format!("{}:{}", prefix, name.local_name))
    } else {
        Cow::Borrowed(&name.local_name)
    }
}

fn start_element_with_optional_name<W: Write>(
    writer: &mut EventWriter<W>,
    elem_name: &str,
    name: Option<&OwnedName>,
) -> Result<(), xml::writer::Error> {
    if let Some(name) = name {
        writer
            .write(XmlEvent::start_element(elem_name).attr("name", format_name(name).as_ref()))?;
    } else {
        writer.write(XmlEvent::start_element(elem_name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::XmlDocumentRoot;
    use crate::ticket::document::{
        reader::ParsableXmlDocument, PrintCapabilitiesDocument, PrintTicketDocument,
    };

    #[test]
    fn serialize_for_print_ticket() {
        let origin = include_bytes!("../../../test_data/print_ticket.xml");
        let origin_document = PrintTicketDocument::parse_from_bytes(origin).unwrap();
        let xml1 = origin_document.to_xml();
        let document = PrintTicketDocument::parse_from_bytes(&xml1).unwrap();
        let xml2 = document.to_xml();
        assert_eq!(xml1, xml2);
    }

    #[test]
    fn serialize_print_capabilities() {
        let origin = include_bytes!("../../../test_data/print_capabilities.xml");
        let origin_document = PrintCapabilitiesDocument::parse_from_bytes(origin).unwrap();
        let xml1 = origin_document.to_xml();
        let document = PrintCapabilitiesDocument::parse_from_bytes(&xml1).unwrap();
        let xml2 = document.to_xml();
        assert_eq!(xml1, xml2);
    }

    #[test]
    fn serialize_qname_value_as_prefixed_name() {
        // As the Microsoft IPP Class Driver lists Letter (storytold/pdfcraft#3823).
        let origin = br#"<psf:PrintTicket version="1" xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:ns0003="http://schemas.microsoft.com/windows/2013/12/printing/printschemaframework2" xmlns:ns0004="urn:only-used-in-a-value">
    <psf:Feature name="psk:PageMediaSize">
        <psf:Option name="psk:NorthAmericaLetter">
            <psf:Property name="ns0003:feedDirectionOverride">
                <psf:Value xsi:type="xsd:QName">psk:LongEdgeFirst</psf:Value>
            </psf:Property>
            <psf:Property name="psk:DisplayName">
                <psf:Value xsi:type="xsd:QName">ns0004:Letter</psf:Value>
            </psf:Property>
        </psf:Option>
    </psf:Feature>
</psf:PrintTicket>"#;
        let document = PrintTicketDocument::parse_from_bytes(origin).unwrap();
        let xml = String::from_utf8(document.to_xml()).unwrap();
        assert!(xml.contains(">psk:LongEdgeFirst<"), "{xml}");
        assert!(xml.contains(">ns0004:Letter<"), "{xml}");
        assert!(xml.contains(r#"xmlns:ns0004="urn:only-used-in-a-value""#), "{xml}");
        assert!(!xml.contains('{'), "{xml}");
    }
}
