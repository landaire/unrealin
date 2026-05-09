using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using UELib;
using UELib.Core;

namespace UnrealinValidator;

internal static class Program
{
    private static int Main(string[] args)
    {
        if (args.Length == 0)
        {
            Console.Error.WriteLine("usage: unrealin-validator <package.u> [<package2.u> ...]");
            Console.Error.WriteLine("       unrealin-validator decompile <package.u> <full.export.path>");
            return 2;
        }

        if (args[0] == "decompile" && args.Length == 3)
        {
            return DecompileOne(args[1], args[2]);
        }

        if (args[0] == "diff" && args.Length == 3)
        {
            return DiffPackages(args[1], args[2]);
        }

        if (args[0] == "list-exports" && args.Length == 2)
        {
            return ListExports(args[1]);
        }

        if (args[0] == "dump-mip" && args.Length >= 3)
        {
            // dump-mip <package.utx> <ExportName> [outDir]
            // Writes <outDir>/<ExportName>.mip<N>.bin for every mip of every
            // texture export whose object name contains <ExportName>.
            return DumpMip(args[1], args[2], args.Length >= 4 ? args[3] : ".");
        }

        int errors = 0;
        foreach (string path in args)
        {
            errors += Validate(path);
        }
        return errors == 0 ? 0 : 1;
    }

    private static int DecompileOne(string path, string exportFullName)
    {
        var pkg = UnrealLoader.LoadPackage(path);
#pragma warning disable CS0618
        pkg.InitializePackage(UnrealPackage.InitFlags.RegisterClasses | UnrealPackage.InitFlags.Construct);
#pragma warning restore CS0618
        // Load every export first so lookup tables populated for the
        // bytecode decompiler. Same flow as Validate so the decompile
        // attempt sees the same state.
        foreach (var exp in pkg.Exports)
        {
            var o = exp.Object;
            if (o is null or UnknownObject) continue;
            try
            {
                if (o.DeserializationState == default && !o.ShouldDeserializeOnDemand)
                {
                    o.Load();
                }
            }
            catch { /* swallow; we just want lookup tables built */ }
        }
        foreach (var exp in pkg.Exports)
        {
            var obj = exp.Object;
            if (obj is null or UnknownObject) continue;
            // Match by GetReferencePath which formats as "Class'package.path'".
            // Accept either bare full path or that formatted form.
            var refPath = obj.GetReferencePath();
            if (refPath != exportFullName && exp.ObjectName != exportFullName
                && !refPath.EndsWith($"'{exportFullName}'"))
            {
                continue;
            }
            if (obj is UStruct us)
            {
                Console.WriteLine(us.Decompile());
                return 0;
            }
        }
        Console.Error.WriteLine($"export not found: {exportFullName}");
        return 1;
    }

    private static int DumpMip(string path, string nameFilter, string outDir)
    {
        Directory.CreateDirectory(outDir);
        var pkg = UnrealLoader.LoadPackage(path);
#pragma warning disable CS0618
        pkg.InitializePackage(UnrealPackage.InitFlags.RegisterClasses | UnrealPackage.InitFlags.Construct);
#pragma warning restore CS0618
        int hits = 0;
        foreach (var exp in pkg.Exports)
        {
            var obj = exp.Object;
            if (obj is null or UnknownObject) continue;
            string objName = exp.ObjectName.ToString() ?? "";
            if (!objName.Contains(nameFilter, StringComparison.OrdinalIgnoreCase)) continue;
            try { obj.Load(); } catch { continue; }
            if (obj is not UELib.Engine.UTexture tex) continue;
            for (int i = 0; i < (tex.Mips?.Count ?? 0); i++)
            {
                var mip = tex.Mips[i];
                // UELib's TLazyArray Deserialize records storage offset/size but
                // skips reading the bytes; LoadData has to be invoked explicitly
                // to populate ElementData.
                if (mip.Data.ElementData == null || mip.Data.ElementData.Length == 0)
                {
                    try { mip.Data.LoadData(pkg.Stream); } catch { continue; }
                }
                var bytes = mip.Data.ElementData;
                if (bytes == null || bytes.Length == 0) continue;
                string fn = Path.Combine(outDir, $"{objName}.mip{i}.bin");
                File.WriteAllBytes(fn, bytes);
                Console.WriteLine($"{objName} mip{i} -> {fn} ({bytes.Length} bytes, {mip.USize}x{mip.VSize})");
                hits++;
            }
        }
        if (hits == 0)
        {
            Console.Error.WriteLine($"no texture matched '{nameFilter}' in {path}");
            return 1;
        }
        return 0;
    }

    private static int ListExports(string path)
    {
        var pkg = UnrealLoader.LoadPackage(path);
#pragma warning disable CS0618
        pkg.InitializePackage(UnrealPackage.InitFlags.RegisterClasses | UnrealPackage.InitFlags.Construct);
#pragma warning restore CS0618
        foreach (var exp in pkg.Exports)
        {
            var obj = exp.Object;
            var refPath = obj?.GetReferencePath() ?? "<null>";
            Console.WriteLine($"{exp.ToString(),-60} class={exp.ClassName ?? "Class"} ref={refPath}");
        }
        return 0;
    }

    private static int DiffPackages(string oursPath, string refPath)
    {
        var ours = UnrealLoader.LoadPackage(oursPath);
        var refPkg = UnrealLoader.LoadPackage(refPath);
        Console.WriteLine($"=== {Path.GetFileName(oursPath)} vs {Path.GetFileName(refPath)} ===");

        Console.WriteLine($"names: ours={ours.Names.Count} ref={refPkg.Names.Count}");
        var oursNames = ours.Names.Select(n => n.Name.ToString()).ToList();
        var refNames = refPkg.Names.Select(n => n.Name.ToString()).ToList();
        var oursSet = new HashSet<string>(oursNames, StringComparer.OrdinalIgnoreCase);
        var refSet = new HashSet<string>(refNames, StringComparer.OrdinalIgnoreCase);
        var onlyOurs = oursSet.Except(refSet, StringComparer.OrdinalIgnoreCase).Take(20).ToList();
        var onlyRef = refSet.Except(oursSet, StringComparer.OrdinalIgnoreCase).Take(20).ToList();
        if (onlyOurs.Count > 0) Console.WriteLine($"  only-in-ours ({onlyOurs.Count}): {string.Join(", ", onlyOurs)}");
        if (onlyRef.Count > 0) Console.WriteLine($"  only-in-ref ({onlyRef.Count}): {string.Join(", ", onlyRef)}");
        int oursNone = oursNames.Count(n => n.Equals("None", StringComparison.OrdinalIgnoreCase));
        int refNone = refNames.Count(n => n.Equals("None", StringComparison.OrdinalIgnoreCase));
        if (oursNone != refNone) Console.WriteLine($"  'None' occurrences: ours={oursNone} ref={refNone}");

        Console.WriteLine($"imports: ours={ours.Imports.Count} ref={refPkg.Imports.Count}");
        var oursImports = ours.Imports.Select(i => $"{i.ClassName.Name}'{i.ObjectName.Name}'").ToHashSet();
        var refImports = refPkg.Imports.Select(i => $"{i.ClassName.Name}'{i.ObjectName.Name}'").ToHashSet();
        var onlyOursI = oursImports.Except(refImports).Take(10).ToList();
        var onlyRefI = refImports.Except(oursImports).Take(10).ToList();
        if (onlyOursI.Count > 0) Console.WriteLine($"  only-in-ours ({onlyOursI.Count}): {string.Join(", ", onlyOursI)}");
        if (onlyRefI.Count > 0) Console.WriteLine($"  only-in-ref ({onlyRefI.Count}): {string.Join(", ", onlyRefI)}");

        Console.WriteLine($"exports: ours={ours.Exports.Count} ref={refPkg.Exports.Count}");
        var oursExports = ours.Exports.Select(e => $"{e.ClassName ?? "Class"}'{e}'").ToHashSet();
        var refExports = refPkg.Exports.Select(e => $"{e.ClassName ?? "Class"}'{e}'").ToHashSet();
        var onlyOursE = oursExports.Except(refExports).Take(10).ToList();
        var onlyRefE = refExports.Except(oursExports).Take(10).ToList();
        if (onlyOursE.Count > 0) Console.WriteLine($"  only-in-ours ({onlyOursE.Count}): {string.Join(", ", onlyOursE)}");
        if (onlyRefE.Count > 0) Console.WriteLine($"  only-in-ref ({onlyRefE.Count}): {string.Join(", ", onlyRefE)}");

        Console.WriteLine($"version: ours={ours.Version}/{ours.LicenseeVersion} ref={refPkg.Version}/{refPkg.LicenseeVersion}");
        Console.WriteLine($"flags: ours=0x{ours.PackageFlags:X} ref=0x{refPkg.PackageFlags:X}");
        return 0;
    }

    private static int Validate(string path)
    {
        Console.WriteLine($"=== {Path.GetFileName(path)} ===");
        UnrealPackage pkg;
        try
        {
            pkg = UnrealLoader.LoadPackage(path);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"  LoadPackage threw: {ex.GetType().Name}: {ex.Message}");
            return 1;
        }

        Console.WriteLine($"  build={pkg.Build} version={pkg.Version} licensee={pkg.LicenseeVersion} exports={pkg.Exports.Count}");

        // Construct objects without forcing deserialize so we can iterate
        // and catch per-export errors with names attached.
#pragma warning disable CS0618
        try
        {
            pkg.InitializePackage(UnrealPackage.InitFlags.RegisterClasses | UnrealPackage.InitFlags.Construct);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"  InitializePackage(Construct) threw: {ex.GetType().Name}: {ex.Message}");
            return 1;
        }
#pragma warning restore CS0618

        int errs = 0;
        int scriptTextDangling = 0;
        int decompileErrs = 0;
        var sample = new List<string>();
        foreach (var exp in pkg.Exports)
        {
            var obj = exp.Object;
            if (obj is null or UnknownObject) continue;
            try
            {
                if (obj.DeserializationState == default && !obj.ShouldDeserializeOnDemand)
                {
                    obj.Load();
                }
            }
            catch (Exception ex)
            {
                errs++;
                if (sample.Count < 10)
                {
                    sample.Add($"    [{exp}] ({obj.Class?.Name ?? "?"}) deserialize: {ex.GetType().Name}: {ex.Message}");
                }
                continue;
            }

            if (obj is UStruct us && us.ScriptText != null)
            {
                scriptTextDangling++;
                if (sample.Count < 10)
                {
                    sample.Add($"    [{exp}] ({obj.Class?.Name ?? "?"}) ScriptText is non-null: {us.ScriptText}");
                }
            }

            // Bytecode decompile check: walks the script tree and serializes
            // it as UnrealScript source. This is the path that fails when
            // the on-disk script bytes contain peek-back artifacts (engine
            // load patterns that the decompiler doesn't reproduce). Run for
            // every UStruct subtype with a non-empty Script.
            if (obj is UStruct uss && uss.ScriptSize > 0)
            {
                try
                {
                    _ = uss.Decompile();
                }
                catch (Exception ex)
                {
                    decompileErrs++;
                    if (sample.Count < 10)
                    {
                        sample.Add($"    [{exp}] ({obj.Class?.Name ?? "?"}) decompile: {ex.GetType().Name}: {ex.Message}");
                    }
                }
            }
        }

        Console.WriteLine($"  deserialize errors: {errs}/{pkg.Exports.Count}; non-null ScriptText: {scriptTextDangling}; decompile errors: {decompileErrs}");
        foreach (var s in sample) Console.WriteLine(s);
        return errs + scriptTextDangling + decompileErrs;
    }
}
