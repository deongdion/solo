//! The models against OpenJDK 21: iteration orders and hash codes below were printed by a Java
//! program doing the same operations (HashMap put in order, then remove).

use javavm::{CallKind, ObjectId, UnhandledPolicy, Value, Vm};

fn vm() -> Vm {
    let mut vm = Vm::new(0x1000_0000, 0x2000_0000);
    java::install(&mut vm);
    vm
}

fn call(vm: &mut Vm, kind: CallKind, class: &str, name: &str, desc: &str, this: Option<ObjectId>, args: &[Value]) -> Value {
    let c = vm.class(class);
    let index = vm.method_id(c, name, desc, kind == CallKind::Static).unwrap();
    let method = vm.method(index).unwrap().clone();
    let result = vm.invoke(kind, &method, this, args).unwrap_or_else(|e| panic!("{class}.{name}{desc}: {e}"));
    if let Some(e) = vm.pending.take() {
        panic!("{class}.{name}{desc} threw {}", vm.describe_throwable(e));
    }
    result
}

fn string(vm: &mut Vm, text: &str) -> Value {
    Value::Object(vm.new_string(text))
}

fn text(vm: &Vm, v: Value) -> String {
    vm.string(v.object().unwrap()).unwrap()
}

/// Walks `map.keySet().iterator()` through the interface method IDs native code uses.
fn keys(vm: &mut Vm, map: ObjectId) -> Vec<Value> {
    let set = call(vm, CallKind::Virtual, "java/util/Map", "keySet", "()Ljava/util/Set;", Some(map), &[]);
    let it = call(vm, CallKind::Virtual, "java/util/Set", "iterator", "()Ljava/util/Iterator;", set.object(), &[]);
    let mut out = Vec::new();
    while call(vm, CallKind::Virtual, "java/util/Iterator", "hasNext", "()Z", it.object(), &[]).as_bool() {
        out.push(call(vm, CallKind::Virtual, "java/util/Iterator", "next", "()Ljava/lang/Object;", it.object(), &[]));
    }
    out
}

#[test]
fn hashmap_iterates_in_openjdk_order() {
    let mut vm = vm();
    let words = [
        "zeta", "alpha", "beta", "gamma", "delta", "epsilon", "eta", "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi",
        "rho", "sigma", "tau", "upsilon", "phi", "chi", "psi", "omega", "한글", "é",
    ];
    let class = vm.class("java/util/HashMap");
    let map = vm.new_object(class);
    call(&mut vm, CallKind::Constructor, "java/util/HashMap", "<init>", "()V", Some(map), &[]);
    for (i, w) in words.iter().enumerate() {
        let k = string(&mut vm, w);
        let v = Value::from_object(java::box_value(&mut vm, Value::Int(i as i32)));
        call(&mut vm, CallKind::Virtual, "java/util/Map", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", Some(map), &[k, v]);
    }
    let pi = string(&mut vm, "pi");
    let removed = call(&mut vm, CallKind::Virtual, "java/util/HashMap", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;", Some(map), &[pi]);
    assert_eq!(java::unbox(&vm, removed.object().unwrap()), Some(Value::Int(15)));
    let order: Vec<String> = keys(&mut vm, map).into_iter().map(|k| text(&vm, k)).collect();
    assert_eq!(
        order.join(","),
        "nu,psi,delta,theta,epsilon,lambda,eta,omicron,alpha,kappa,beta,zeta,sigma,iota,chi,upsilon,mu,tau,é,omega,phi,xi,rho,gamma,한글"
    );

    let mut entries = Vec::new();
    for i in [100, -5, 17, 33, 1 << 20, 64, 0, 7] {
        entries.push((Value::Int(i), Value::Null));
    }
    entries.push((Value::Long(3_000_000_000), Value::Null));
    entries.push((Value::Boolean(true), Value::Null));
    let map = java::new_map(&mut vm, java::MapKind::Hash, &entries);
    let order: Vec<String> = keys(&mut vm, map)
        .into_iter()
        .map(|k| match java::unbox(&vm, k.object().unwrap()).unwrap() {
            Value::Boolean(b) => b.to_string(),
            v => v.as_i64().to_string(),
        })
        .collect();
    assert_eq!(order.join(","), "1048576,64,0,3000000000,17,33,100,-5,7,true");
}

#[test]
fn hash_codes_match_java() {
    let mut vm = vm();
    let hash = |vm: &mut Vm, v: Value| {
        let id = java::box_value(vm, v).unwrap();
        call(vm, CallKind::Virtual, "java/lang/Object", "hashCode", "()I", Some(id), &[]).as_i32()
    };
    let hello = string(&mut vm, "hello");
    let korean = string(&mut vm, "한글");
    assert_eq!(hash(&mut vm, hello), 99162322);
    assert_eq!(hash(&mut vm, korean), 1737764);
    assert_eq!(hash(&mut vm, Value::Long(3_000_000_000)), -1294967296);
    assert_eq!(hash(&mut vm, Value::Double(1.5)), 1073217536);
    assert_eq!(hash(&mut vm, Value::Float(1.5)), 1069547520);
}

#[test]
fn map_entries_and_values_through_interfaces() {
    let mut vm = vm();
    let entries = [(Value::Int(1), Value::Int(10)), (Value::Int(2), Value::Int(20))];
    let map = java::new_map(&mut vm, java::MapKind::Linked, &entries);
    let set = call(&mut vm, CallKind::Virtual, "java/util/Map", "entrySet", "()Ljava/util/Set;", Some(map), &[]);
    let array = call(&mut vm, CallKind::Virtual, "java/util/Collection", "toArray", "()[Ljava/lang/Object;", set.object(), &[]);
    let javavm::Array::Object(items) = vm.array(array.object().unwrap()).unwrap().clone() else { panic!() };
    let mut seen = Vec::new();
    for entry in items {
        let k = call(&mut vm, CallKind::Virtual, "java/util/Map$Entry", "getKey", "()Ljava/lang/Object;", entry, &[]);
        let v = call(&mut vm, CallKind::Virtual, "java/util/Map$Entry", "getValue", "()Ljava/lang/Object;", entry, &[]);
        let k = call(&mut vm, CallKind::Virtual, "java/lang/Integer", "intValue", "()I", k.object(), &[]);
        let v = call(&mut vm, CallKind::Virtual, "java/lang/Number", "intValue", "()I", v.object(), &[]);
        seen.push((k.as_i32(), v.as_i32()));
    }
    assert_eq!(seen, vec![(1, 10), (2, 20)]);
    let key = Value::from_object(java::box_value(&mut vm, Value::Int(2)));
    let got = call(&mut vm, CallKind::Virtual, "java/util/Map", "get", "(Ljava/lang/Object;)Ljava/lang/Object;", Some(map), &[key]);
    assert_eq!(java::unbox(&vm, got.object().unwrap()), Some(Value::Int(20)));
    let missing = Value::from_object(java::box_value(&mut vm, Value::Long(2)));
    let has = call(&mut vm, CallKind::Virtual, "java/util/Map", "containsKey", "(Ljava/lang/Object;)Z", Some(map), &[missing]);
    assert_eq!(has, Value::Boolean(false), "Long(2) is not Integer(2)");
    assert_eq!(call(&mut vm, CallKind::Virtual, "java/util/Map", "size", "()I", Some(map), &[]), Value::Int(2));
    // The heap keeps keys and values alive through the map's host data.
    let freed = vm.collect(&[map]);
    assert!(freed > 0);
    assert_eq!(java::map_entries(&vm, map).unwrap().len(), 2);
    let k = java::map_entries(&vm, map).unwrap()[0].0.unwrap();
    assert_eq!(java::unbox(&vm, k), Some(Value::Int(1)));
}

#[test]
fn strings_builders_and_numbers() {
    let mut vm = vm();
    let s = string(&mut vm, "Héllo, 한글");
    let utf8 = string(&mut vm, "UTF-8");
    let bytes = call(&mut vm, CallKind::Virtual, "java/lang/String", "getBytes", "(Ljava/lang/String;)[B", s.object(), &[utf8]);
    assert_eq!(vm.bytes(bytes.object().unwrap()).unwrap(), "Héllo, 한글".as_bytes());

    let class = vm.class("java/lang/String");
    let made = vm.new_object(class);
    call(&mut vm, CallKind::Constructor, "java/lang/String", "<init>", "([BLjava/lang/String;)V", Some(made), &[bytes, utf8]);
    assert_eq!(vm.string(made).unwrap(), "Héllo, 한글");

    let class = vm.class("java/lang/StringBuilder");
    let sb = vm.new_object(class);
    call(&mut vm, CallKind::Constructor, "java/lang/StringBuilder", "<init>", "()V", Some(sb), &[]);
    let x = string(&mut vm, "x=");
    for (desc, arg) in [
        ("(Ljava/lang/String;)Ljava/lang/StringBuilder;", x),
        ("(I)Ljava/lang/StringBuilder;", Value::Int(-3)),
        ("(C)Ljava/lang/StringBuilder;", Value::Char(b',' as u16)),
        ("(D)Ljava/lang/StringBuilder;", Value::Double(1e7)),
        ("(Z)Ljava/lang/StringBuilder;", Value::Boolean(true)),
        ("(J)Ljava/lang/StringBuilder;", Value::Long(1 << 40)),
    ] {
        call(&mut vm, CallKind::Virtual, "java/lang/StringBuilder", "append", desc, Some(sb), &[arg]);
    }
    let out = call(&mut vm, CallKind::Virtual, "java/lang/StringBuilder", "toString", "()Ljava/lang/String;", Some(sb), &[]);
    assert_eq!(text(&vm, out), "x=-3,1.0E7true1099511627776");

    let n = string(&mut vm, "-123");
    assert_eq!(call(&mut vm, CallKind::Static, "java/lang/Integer", "parseInt", "(Ljava/lang/String;)I", None, &[n]), Value::Int(-123));
    let bad = string(&mut vm, "12x");
    let c = vm.class("java/lang/Integer");
    let index = vm.method_id(c, "parseInt", "(Ljava/lang/String;)I", true).unwrap();
    let method = vm.method(index).unwrap().clone();
    vm.invoke(CallKind::Static, &method, None, &[bad]).unwrap();
    let thrown = vm.pending.take().expect("NumberFormatException");
    assert_eq!(vm.describe_throwable(thrown), "java.lang.NumberFormatException: For input string: \"12x\"");
    let nfe = vm.find_class("java/lang/NumberFormatException").unwrap();
    let iae = vm.find_class("java/lang/IllegalArgumentException").unwrap();
    assert!(vm.is_assignable(nfe, iae));
}

#[test]
fn unknown_methods_stay_unhandled() {
    let mut vm = vm();
    vm.policy = UnhandledPolicy::Stop;
    let s = string(&mut vm, "abc");
    let c = vm.class("java/lang/String");
    let index = vm.method_id(c, "matches", "(Ljava/lang/String;)Z", false).unwrap();
    let method = vm.method(index).unwrap().clone();
    let error = vm.invoke(CallKind::Virtual, &method, s.object(), &[s]).unwrap_err();
    assert_eq!(error.to_string(), "unhandled JNI callback: java/lang/String->matches(Ljava/lang/String;)Z");
}
