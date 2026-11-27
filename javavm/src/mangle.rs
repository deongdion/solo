//! Names of statically registered natives: `Java_<class>_<method>` and, for overloads,
//! `Java_<class>_<method>__<parameters>` (JNI specification, "Resolving Native Method Names").

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '/' | '.' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("_0{u:04x}"));
                }
            }
        }
    }
}

/// The short name, `Java_com_example_Foo_bar`.
pub fn short_name(class: &str, method: &str) -> String {
    let mut out = String::from("Java_");
    escape(class, &mut out);
    out.push('_');
    escape(method, &mut out);
    out
}

/// The long name with the parameter descriptor, `Java_com_example_Foo_bar__ILjava_lang_String_2`.
pub fn long_name(class: &str, method: &str, desc: &str) -> String {
    let mut out = short_name(class, method);
    out.push_str("__");
    let params = desc.strip_prefix('(').and_then(|d| d.split(')').next()).unwrap_or("");
    escape(params, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangles_like_the_specification() {
        assert_eq!(short_name("com/example/My_Class", "do_it"), "Java_com_example_My_1Class_do_1it");
        assert_eq!(long_name("a/B", "f", "(I[Ljava/lang/String;)V"), "Java_a_B_f__I_3Ljava_lang_String_2");
        assert_eq!(short_name("a/Ü", "m"), "Java_a__000dc_m");
    }
}
