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
            return 2;
        }

        int errors = 0;
        foreach (string path in args)
        {
            errors += Validate(path);
        }
        return errors == 0 ? 0 : 1;
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
        var sample = new List<string>();
        foreach (var exp in pkg.Exports)
        {
            var obj = exp.Object;
            if (obj is null || obj is UnknownObject) continue;
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
                    sample.Add($"    [{exp}] ({obj.Class?.Name ?? "?"}) {ex.GetType().Name}: {ex.Message}");
                }
                continue;
            }

            // UExplorer-style follow-the-reference check: if this is a UStruct,
            // its ScriptText (a UTextBuffer*) should be null in our re-emit.
            // A non-null reference into an export with empty body is what
            // triggered UExplorer's NullReferenceException.
            if (obj is UStruct us && us.ScriptText != null)
            {
                scriptTextDangling++;
                if (sample.Count < 10)
                {
                    sample.Add($"    [{exp}] ({obj.Class?.Name ?? "?"}) ScriptText is non-null: {us.ScriptText}");
                }
            }
        }

        Console.WriteLine($"  deserialize errors: {errs}/{pkg.Exports.Count}; non-null ScriptText: {scriptTextDangling}");
        foreach (var s in sample) Console.WriteLine(s);
        return errs + scriptTextDangling;
    }
}
