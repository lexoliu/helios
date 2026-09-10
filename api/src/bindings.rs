use crate::wit_bindgen;

#[cfg(any(
    all(feature = "instances", feature = "http-handler"),
    all(feature = "instances", feature = "compositor"),
    all(feature = "http-handler", feature = "compositor"),
))]
compile_error!(
    "a component implements exactly one world: `instances` selects `debugger`, `http-handler` selects `http-handler` and `compositor` selects `compositor`, so enable at most one"
);

#[cfg(feature = "compositor")]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "compositor",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(all(feature = "http-handler", not(feature = "compositor")))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "http-handler",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(all(
    feature = "instances",
    not(feature = "http-handler"),
    not(feature = "compositor")
))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "debugger",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(all(
    not(feature = "instances"),
    not(feature = "http-handler"),
    not(feature = "compositor")
))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "init",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});
