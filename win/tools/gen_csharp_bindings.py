#!/usr/bin/env python3
"""CrossMail Windows 侧 C# 绑定层机械展开器（自写，无第三方生成器）。

只读输入：crossmail_ffi.udl（唯一契约）+ UniFFI 生成的 Python 绑定（C ABI 真值参考）。
输出：可审查的 C# 源码 + abi-symbols.json。EntryPoint 最终由 dumpbin /exports 真值表核对。
"""
import argparse
import json
import re
from pathlib import Path

PRIM = {
    "string": "string", "boolean": "bool", "u8": "byte", "u16": "ushort",
    "u32": "uint", "u64": "ulong", "i32": "int", "i64": "long",
    "timestamp": "DateTimeOffset",
}
VALUE_TYPES = {"bool", "byte", "ushort", "uint", "ulong", "int", "long", "DateTimeOffset"}
CTYPES_TO_CS = {
    "ctypes.c_void_p": "IntPtr",
    "_UniffiRustBuffer": "RustBuffer",
    "_UniffiForeignBytes": "ForeignBytes",
    "ctypes.POINTER(_UniffiRustCallStatus)": "ref RustCallStatus",
    "ctypes.POINTER(_UniffiVTableCallbackInterfacePushObserver)": "IntPtr",
    "ctypes.c_int8": "sbyte",
    "ctypes.c_uint8": "byte",
    "ctypes.c_int16": "short",
    "ctypes.c_uint16": "ushort",
    "ctypes.c_int32": "int",
    "ctypes.c_uint32": "uint",
    "ctypes.c_int64": "long",
    "ctypes.c_uint64": "ulong",
    "ctypes.c_float": "float",
    "ctypes.c_double": "double",
    "None": "void",
}


def pascal(name):
    return "".join(p[:1].upper() + p[1:] for p in name.split("_") if p)


def camel(name):
    p = pascal(name)
    return p[:1].lower() + p[1:]


def strip_comments(text):
    return "\n".join(line.split("//")[0] for line in text.splitlines())


def parse_params(body):
    out = []
    for raw in body.split(","):
        p = " ".join(raw.split())
        if not p:
            continue
        m = re.match(r"^(.+?)\s+(\w+)$", p)
        if not m:
            raise ValueError("bad param: " + p)
        out.append((m.group(1).strip(), m.group(2)))
    return out


def parse_methods(body):
    out = []
    for raw in body.split(";"):
        s = " ".join(raw.split())
        if not s:
            continue
        throws = False
        m = re.match(r"^\[Throws=(\w+)\]\s*(.*)$", s)
        if m:
            throws = True
            s = m.group(2)
        m = re.match(r"^(.+?)\s+(\w+)\s*\((.*)\)$", s)
        if not m:
            raise ValueError("bad method: " + s)
        out.append({
            "ret": m.group(1).strip(),
            "name": m.group(2),
            "params": parse_params(m.group(3)),
            "throws": throws,
        })
    return out


def parse_udl(text):
    t = strip_comments(text)
    dictionaries = {}
    for m in re.finditer(r"dictionary\s+(\w+)\s*\{(.*?)\}", t, re.S):
        fields = []
        for raw in m.group(2).split(";"):
            f = " ".join(raw.split())
            if not f:
                continue
            fm = re.match(r"^(.+?)\s+(\w+)$", f)
            if not fm:
                raise ValueError("bad field: " + f)
            fields.append((fm.group(1).strip(), fm.group(2)))
        dictionaries[m.group(1)] = fields

    enums = {}
    for m in re.finditer(r"(?<![\[\w])enum\s+(\w+)\s*\{(.*?)\}", t, re.S):
        enums[m.group(1)] = re.findall(r'"(\w+)"', m.group(2))

    variants = {}
    for kind, tag in (("Enum", "enum_variants"), ("Error", "error_variants")):
        m = re.search(r"\[" + kind + r"\]\s*interface\s+(\w+)\s*\{(.*?)\}", t, re.S)
        if m:
            items = []
            for raw in m.group(2).split(";"):
                v = " ".join(raw.split())
                if not v:
                    continue
                vm = re.match(r"^(\w+)\s*\((.*)\)$", v)
                if not vm:
                    raise ValueError("bad variant: " + v)
                has_payload = bool(vm.group(2).strip())
                items.append((vm.group(1), has_payload))
            variants[tag] = (m.group(1), items)

    variant_names = {v[0] for v in variants.values()}
    interfaces = {}
    for m in re.finditer(r"^\s*(?:callback\s+)?interface\s+(\w+)\s*\{(.*?)\}", t, re.S | re.M):
        if m.group(1) in variant_names:
            continue
        interfaces[m.group(1)] = parse_methods(m.group(2))

    nsm = re.search(r"namespace\s+crossmail_ffi\s*\{(.*?)\}", t, re.S)
    if not nsm:
        raise ValueError("namespace crossmail_ffi missing")
    return {
        "dictionaries": dictionaries,
        "enums": enums,
        "variants": variants,
        "interfaces": interfaces,
        "functions": parse_methods(nsm.group(1)),
    }


def cs_type(t, model):
    optional = t.endswith("?")
    base = t[:-1] if optional else t
    seq = re.match(r"^sequence<(.+)>$", base)
    if seq:
        inner = cs_type(seq.group(1), model)
        c = "IReadOnlyList<" + inner + ">"
    elif base == "PushObserver":
        c = "IPushObserver"
    elif base in PRIM:
        c = PRIM[base]
    elif base in model["enums"] or base in model["dictionaries"] or base in ("FlagDto", "CrossMailError"):
        c = base
    else:
        c = base
    if optional:
        return c + "?" if (c in VALUE_TYPES or seq) else c + "?"
    return c


def write_expr(t, expr, model, w="w"):
    optional = t.endswith("?")
    base = t[:-1] if optional else t
    seq = re.match(r"^sequence<(.+)>$", base)
    if seq:
        inner = write_expr(seq.group(1), "x", model, w="bw")
        return "Codec.WriteSequence(" + w + ", " + expr + ", (bw, x) => { " + inner + " });"
    if base == "string":
        return "Codec.Write" + ("Optional" if optional else "") + "String(" + w + ", " + expr + ");"
    if base == "boolean":
        return "Codec.WriteBool(" + w + ", " + expr + ");"
    if base == "timestamp":
        return "Codec.Write" + ("Optional" if optional else "") + "Timestamp(" + w + ", " + expr + ");"
    if base in ("u8", "u16", "u32", "u64", "i32", "i64"):
        suffix = {"u8": "U8", "u16": "U16", "u32": "U32", "u64": "U64", "i32": "I32", "i64": "I64"}[base]
        return "Codec.Write" + ("Optional" if optional else "") + suffix + "(" + w + ", " + expr + ");"
    if base in model["enums"]:
        cast = "(int?)" if optional else "(int)"
        return "Codec.Write" + ("Optional" if optional else "") + "Enum(" + w + ", " + cast + expr + ");"
    if base == "FlagDto":
        return expr + ".Write(" + w + ");"
    if base in model["dictionaries"]:
        if optional:
            return "Codec.WriteOptional(" + w + ", " + expr + ", (bw, x) => x.Write(bw));"
        return expr + ".Write(" + w + ");"
    raise ValueError("unsupported write type: " + t)


def read_expr(t, model, r="r"):
    optional = t.endswith("?")
    base = t[:-1] if optional else t
    seq = re.match(r"^sequence<(.+)>$", base)
    if seq:
        return "Codec.ReadSequence(" + r + ", br => " + read_expr(seq.group(1), model, r="br") + ")"
    if base == "string":
        return "Codec.Read" + ("Optional" if optional else "") + "String(" + r + ")"
    if base == "boolean":
        return "Codec.ReadBool(" + r + ")"
    if base == "timestamp":
        return "Codec.Read" + ("Optional" if optional else "") + "Timestamp(" + r + ")"
    if base in ("u8", "u16", "u32", "u64", "i32", "i64"):
        suffix = {"u8": "U8", "u16": "U16", "u32": "U32", "u64": "U64", "i32": "I32", "i64": "I64"}[base]
        return "Codec.Read" + ("Optional" if optional else "") + suffix + "(" + r + ")"
    if base in model["enums"]:
        if optional:
            return "Codec.ReadOptionalEnum<" + base + ">(" + r + ")"
        return "(" + base + ")Codec.ReadEnum(" + r + ")"
    if base == "FlagDto":
        return "global::CrossMail.Bindings.FlagDto.Read(" + r + ")"
    if base in model["dictionaries"]:
        # 顶层函数 CoreStatus() 与 DTO 类型 CoreStatus 同名，必须用完全限定名避开方法组解析。
        ns = "global::CrossMail.Bindings." + base
        if optional:
            return "Codec.ReadOptional(" + r + ", br => " + ns + ".Read(br))"
        return ns + ".Read(" + r + ")"
    raise ValueError("unsupported read type: " + t)


def gen_models(model):
    L = []
    L.append("// <auto-generated>由 win/tools/gen_csharp_bindings.py 从 crates/ffi/src/crossmail_ffi.udl 机械展开。</auto-generated>")
    L.append("// 字段顺序与 UDL 一致；序列化为 UniFFI 0.28 大端布局。请勿手工编辑，改契约请重跑生成器。")
    L.append("using System;")
    L.append("using System.Collections.Generic;")
    L.append("namespace CrossMail.Bindings;")
    L.append("")
    for name, variants in model["enums"].items():
        L.append("public enum " + name)
        L.append("{")
        for i, v in enumerate(variants, 1):
            L.append("    " + pascal(v) + " = " + str(i) + ",")
        L.append("}")
        L.append("")
    enum_variants = model["variants"].get("enum_variants")
    if enum_variants:
        vname, items = enum_variants
        L.append("public abstract class " + vname)
        L.append("{")
        L.append("    public abstract int Tag { get; }")
        for i, (v, has_payload) in enumerate(items, 1):
            L.append("    public sealed class " + pascal(v) + " : " + vname)
            L.append("    {")
            L.append("        public override int Tag => " + str(i) + ";")
            if has_payload:
                L.append("        public string Value { get; }")
                L.append("        public " + pascal(v) + "(string value) { Value = value; }")
            L.append("    }")
        L.append("    internal void Write(BeWriter w)")
        L.append("    {")
        L.append("        w.WriteI32(Tag);")
        for v, has_payload in items:
            if has_payload:
                L.append("        if (this is " + pascal(v) + " c) Codec.WriteString(w, c.Value);")
        L.append("    }")
        L.append("    internal static " + vname + " Read(BeReader r)")
        L.append("    {")
        L.append("        int tag = r.ReadI32();")
        L.append("        switch (tag)")
        L.append("        {")
        for i, (v, has_payload) in enumerate(items, 1):
            val = "new " + pascal(v) + "(Codec.ReadString(r))" if has_payload else "new " + pascal(v) + "()"
            L.append("            case " + str(i) + ": return " + val + ";")
        L.append("            default: throw new CrossMailBindingException(\"invalid " + vname + " tag: \" + tag);")
        L.append("        }")
        L.append("    }")
        L.append("}")
        L.append("")
    for name, fields in model["dictionaries"].items():
        L.append("public sealed class " + name)
        L.append("{")
        for t, f in fields:
            L.append("    public " + cs_type(t, model) + " " + pascal(f) + " { get; set; } = default!;")
        L.append("")
        L.append("    internal void Write(BeWriter w)")
        L.append("    {")
        for t, f in fields:
            L.append("        " + write_expr(t, pascal(f), model))
        L.append("    }")
        L.append("")
        L.append("    internal static " + name + " Read(BeReader r)")
        L.append("    {")
        L.append("        return new " + name)
        L.append("        {")
        for t, f in fields:
            L.append("            " + pascal(f) + " = " + read_expr(t, model) + ",")
        L.append("        };")
        L.append("    }")
        L.append("}")
        L.append("")
    return "\n".join(L) + "\n"


def gen_errors(model):
    ev = model["variants"].get("error_variants")
    if not ev:
        raise ValueError("error interface missing")
    name, items = ev
    L = ["// <auto-generated>错误映射，变体编号与 UniFFI scaffolding 一致（1-based）。</auto-generated>",
         "using System;", "namespace CrossMail.Bindings;", ""]
    L.append("public enum " + name + "Kind")
    L.append("{")
    for i, (v, _) in enumerate(items, 1):
        L.append("    " + v + " = " + str(i) + ",")
    L.append("}")
    L.append("")
    L.append("public sealed class CrossMailException : Exception")
    L.append("{")
    L.append("    public " + name + "Kind Kind { get; }")
    L.append("    public CrossMailException(" + name + "Kind kind, string message) : base(message) { Kind = kind; }")
    L.append("    internal static CrossMailException Read(BeReader r)")
    L.append("    {")
    L.append("        int tag = r.ReadI32();")
    L.append("        string message = Codec.ReadString(r);")
    L.append("        return new CrossMailException((" + name + "Kind)tag, message);")
    L.append("    }")
    L.append("}")
    L.append("")
    return "\n".join(L) + "\n"


def parse_ffi_decls(py_text):
    # 按行解析：argtypes 元组里的 ctypes.POINTER(...) 自带括号，正则非贪婪匹配会被截断。
    argtypes = {}
    lines = py_text.splitlines()
    i = 0
    while i < len(lines):
        m = re.match(r"^\s*_UniffiLib\.(\w+)\.argtypes\s*=\s*\((.*)$", lines[i])
        if m:
            sym = m.group(1)
            raw = []

            def collect(line):
                for part in line.split(","):
                    part = part.strip()
                    if part:
                        raw.append(part)

            tail = m.group(2).strip()
            if tail.endswith(")"):
                collect(tail[:-1])
            else:
                if tail:
                    collect(tail)
                i += 1
                while i < len(lines) and not re.match(r"^\s*\)\s*$", lines[i]):
                    collect(lines[i])
                    i += 1
            argtypes[sym] = raw
        i += 1
    restypes = {}
    for m in re.finditer(r"_UniffiLib\.(\w+)\.restype\s*=\s*([^\n]+)", py_text):
        restypes[m.group(1)] = m.group(2).strip()
    checksums = {}
    for m in re.finditer(r"lib\.(uniffi_crossmail_ffi_checksum_\w+)\(\)\s*!=\s*(\d+)", py_text):
        checksums[m.group(1)] = int(m.group(2))
    return argtypes, restypes, checksums


def gen_native(argtypes, restypes, checksums):
    keep = []
    for sym in sorted(set(list(argtypes) + list(checksums) + ["ffi_crossmail_ffi_uniffi_contract_version"])):
        if sym.startswith("uniffi_crossmail_ffi_fn_") or sym.startswith("uniffi_crossmail_ffi_checksum_") \
           or sym.startswith("ffi_crossmail_ffi_rustbuffer_") or sym == "ffi_crossmail_ffi_uniffi_contract_version":
            keep.append(sym)
    L = ["// <auto-generated>C ABI 声明。EntryPoint 由 DLL 导出表真值核对，见 win/scripts/check-csharp-bindings.ps1。</auto-generated>",
         "using System;", "using System.Runtime.InteropServices;", "namespace CrossMail.Bindings;", "",
         "internal static class NativeMethods", "{",
         "    internal const string DllName = \"crossmail_ffi\";", ""]
    used = []
    for sym in keep:
        if sym in checksums:
            args, ret = [], "ushort"
        elif sym == "ffi_crossmail_ffi_uniffi_contract_version":
            args, ret = [], "uint"
        else:
            if sym not in argtypes:
                continue
            raw_args = argtypes[sym]
            args = []
            skip = False
            for a in raw_args:
                if a not in CTYPES_TO_CS:
                    skip = True
                    break
                args.append(CTYPES_TO_CS[a])
            if skip:
                continue
            rt = restypes.get(sym, "None")
            if rt not in CTYPES_TO_CS:
                continue
            ret = CTYPES_TO_CS[rt]
        sig = ", ".join(a + " p" + str(i) for i, a in enumerate(args)) if args else ""
        L.append("    [DllImport(DllName, EntryPoint = \"" + sym + "\", CallingConvention = CallingConvention.Cdecl)]")
        L.append("    internal static extern " + ret + " " + sym + "(" + sig + ");")
        L.append("")
        used.append(sym)
    L.append("}")
    L.append("")
    return "\n".join(L) + "\n", used


def native_arg_expr(argtype, t, pname, model):
    if argtype == "_UniffiRustBuffer":
        # UniFFI: String.lower() 写纯 UTF-8 字节（无 i32 长度前缀），与嵌套 write() 不同。
        if t == "string":
            return "Codec.LowerString(" + pname + ")"
        return "Codec.Lower(w => { " + write_expr(t, pname, model, w="w") + " })"
    if argtype == "ctypes.c_int8":
        return "(sbyte)(" + pname + " ? 1 : 0)"
    if argtype == "ctypes.c_uint32":
        return "(uint)" + pname
    if argtype == "ctypes.c_uint64":
        return "PushObserverRegistry.Register(" + pname + ")"
    if argtype == "ctypes.c_void_p":
        return pname
    raise ValueError("unsupported argtype " + argtype + " for " + t)


def gen_engine(model, argtypes, restypes):
    L = ["// <auto-generated>CrossMailEngine / 顶层函数 / PushSubscription 托管包装。所有接口同步阻塞，UI 必须放后台线程。</auto-generated>",
         "using System;", "using System.Collections.Generic;", "namespace CrossMail.Bindings;", ""]
    L.append("public sealed class CrossMailEngine : IDisposable")
    L.append("{")
    L.append("    private readonly CrossMailEngineHandle _handle;")
    L.append("    internal CrossMailEngine(CrossMailEngineHandle handle) { _handle = handle; }")
    L.append("    public void Dispose() => _handle.Dispose();")
    L.append("")

    methods = model["interfaces"]["CrossMailEngine"]
    for meth in methods:
        ret, name, params, throws = meth["ret"], meth["name"], meth["params"], meth["throws"]
        sym = "uniffi_crossmail_ffi_fn_method_crossmailengine_" + name
        raw_args = argtypes.get(sym)
        if raw_args is None:
            raise ValueError("missing native decl: " + sym)
        abi_ret = restypes.get(sym)
        cs_ret = "void" if ret == "void" else cs_type(ret, model)
        cs_params = ", ".join(cs_type(t, model) + " " + camel(n) for t, n in params)
        L.append("    public " + cs_ret + " " + pascal(name) + "(" + cs_params + ")")
        L.append("    {")
        # native args: handle + middle params + status
        middle = raw_args[1:-1]
        if len(middle) != len(params):
            raise ValueError("arity mismatch " + sym + ": " + str(len(middle)) + " vs " + str(len(params)))
        # 与 Python 绑定 self._uniffi_clone_pointer() 对齐：每次对象方法调用前 clone 一份
        # Arc 引用，clone 指针由 Rust scaffolding 消费。直接复用原始指针会 use-after-free。
        native_args = ["RustCall.CloneEngine(_handle)"]
        for (t, n), a in zip(params, middle):
            native_args.append(native_arg_expr(a, t, camel(n), model))
        native_args.append("ref box.Status")
        call = "NativeMethods." + sym + "(" + ", ".join(native_args) + ")"
        call_lambda = "box => " + call
        if abi_ret == "_UniffiRustBuffer":
            if ret == "void":
                L.append("        RustCall.InvokeBuffer(" + call_lambda + ", _ => (object?)null, " + str(throws).lower() + ");")
            else:
                lift_expr = "Codec.ReadRawString(r)" if ret == "string" else read_expr(ret, model)
            L.append("        return RustCall.InvokeBuffer(" + call_lambda + ", r => " + lift_expr + ", " + str(throws).lower() + ");")
        elif abi_ret == "ctypes.c_void_p":
            handle_cls = "PushSubscriptionHandle" if ret == "PushSubscription" else "CrossMailEngineHandle"
            L.append("        IntPtr ptr = RustCall.InvokeScalar(" + call_lambda + ", " + str(throws).lower() + ");")
            L.append("        return new " + ("PushSubscription" if ret == "PushSubscription" else "CrossMailEngine") + "(new " + handle_cls + "(ptr));")
        elif abi_ret == "ctypes.c_int8" and ret == "boolean":
            L.append("        return RustCall.InvokeScalar(" + call_lambda + ", " + str(throws).lower() + ") != 0;")
        elif abi_ret in ("ctypes.c_uint32", "ctypes.c_int8", "ctypes.c_uint64"):
            L.append("        return RustCall.InvokeScalar(" + call_lambda + ", " + str(throws).lower() + ");")
        elif abi_ret == "None" or ret == "void":
            L.append("        RustCall.InvokeVoid(" + call_lambda + ", " + str(throws).lower() + ");")
        else:
            raise ValueError("unsupported restype " + str(abi_ret) + " for " + sym)
        L.append("    }")
        L.append("")
    L.append("}")
    L.append("")

    # PushSubscription
    L.append("public sealed class PushSubscription : IDisposable")
    L.append("{")
    L.append("    private readonly PushSubscriptionHandle _handle;")
    L.append("    internal PushSubscription(PushSubscriptionHandle handle) { _handle = handle; }")
    L.append("    public void Dispose() => _handle.Dispose();")
    L.append("    public void Cancel() => RustCall.InvokeVoid(box => NativeMethods.uniffi_crossmail_ffi_fn_method_pushsubscription_cancel(RustCall.CloneSubscription(_handle), ref box.Status), false);")
    L.append("    public bool IsCancelled() => RustCall.InvokeScalar(box => NativeMethods.uniffi_crossmail_ffi_fn_method_pushsubscription_is_cancelled(RustCall.CloneSubscription(_handle), ref box.Status), false) != 0;")
    L.append("}")
    L.append("")

    # 顶层函数
    L.append("public static class CrossMailCore")
    L.append("{")
    for meth in model["functions"]:
        ret, name, params, throws = meth["ret"], meth["name"], meth["params"], meth["throws"]
        sym = "uniffi_crossmail_ffi_fn_func_" + name
        raw_args = argtypes.get(sym)
        if raw_args is None:
            raise ValueError("missing native decl: " + sym)
        abi_ret = restypes.get(sym)
        cs_ret = "void" if ret == "void" else cs_type(ret, model)
        cs_params = ", ".join(cs_type(t, model) + " " + camel(n) for t, n in params)
        L.append("    public static " + cs_ret + " " + pascal(name) + "(" + cs_params + ")")
        L.append("    {")
        middle = raw_args[:-1]
        if len(middle) != len(params):
            raise ValueError("arity mismatch " + sym)
        native_args = []
        for (t, n), a in zip(params, middle):
            native_args.append(native_arg_expr(a, t, camel(n), model))
        native_args.append("ref box.Status")
        call_lambda = "box => NativeMethods." + sym + "(" + ", ".join(native_args) + ")"
        if abi_ret == "_UniffiRustBuffer":
            lift_expr = "Codec.ReadRawString(r)" if ret == "string" else read_expr(ret, model)
            L.append("        return RustCall.InvokeBuffer(" + call_lambda + ", r => " + lift_expr + ", " + str(throws).lower() + ");")
        elif abi_ret == "ctypes.c_void_p":
            L.append("        IntPtr ptr = RustCall.InvokeScalar(" + call_lambda + ", " + str(throws).lower() + ");")
            L.append("        return new CrossMailEngine(new CrossMailEngineHandle(ptr));")
        else:
            raise ValueError("unsupported func restype " + str(abi_ret))
        L.append("    }")
        L.append("")
    L.append("}")
    L.append("")
    return "\n".join(L) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--udl", required=True)
    ap.add_argument("--py-binding", required=True)
    ap.add_argument("--out-dir", required=True)
    args = ap.parse_args()

    udl_text = Path(args.udl).read_text(encoding="utf-8")
    py_text = Path(args.py_binding).read_text(encoding="utf-8")
    model = parse_udl(udl_text)
    argtypes, restypes, checksums = parse_ffi_decls(py_text)
    native_src, used_symbols = gen_native(argtypes, restypes, checksums)

    out = Path(args.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    generated = {
        "Models.g.cs": gen_models(model),
        "Errors.g.cs": gen_errors(model),
        "NativeSymbols.g.cs": native_src,
        "Engine.g.cs": gen_engine(model, argtypes, restypes),
    }
    for f, src in generated.items():
        (out / f).write_text("#nullable enable\n" + src, encoding="utf-8")
    manifest = {
        "udl_dictionaries": sorted(model["dictionaries"]),
        "udl_enums": sorted(model["enums"]),
        "udl_functions": [m["name"] for m in model["functions"]],
        "udl_engine_methods": [m["name"] for m in model["interfaces"]["CrossMailEngine"]],
        "udl_error_variants": [v for v, _ in model["variants"]["error_variants"][1]],
        "native_entrypoints": used_symbols,
        "checksums": checksums,
        "bindings_contract_version": 26,
    }
    (out / "abi-symbols.json").write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print("[OK] dictionaries=%d enums=%d functions=%d engine_methods=%d errors=%d entrypoints=%d checksums=%d" % (
        len(model["dictionaries"]), len(model["enums"]), len(model["functions"]),
        len(model["interfaces"]["CrossMailEngine"]), len(model["variants"]["error_variants"][1]),
        len(used_symbols), len(checksums)))
    for f in ("Models.g.cs", "Errors.g.cs", "NativeSymbols.g.cs", "Engine.g.cs", "abi-symbols.json"):
        print("  %-24s %6d lines" % (f, len((out / f).read_text(encoding="utf-8").splitlines())))


if __name__ == "__main__":
    main()
