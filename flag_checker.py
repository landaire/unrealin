#!/usr/bin/env python3
import re
import sys

# EObjectFlags enum
flags_dict = {
    0x00000001: "RF_Transactional",
    0x00000002: "RF_Unreachable",
    0x00000004: "RF_Public",
    0x00000008: "RF_TagImp",
    0x00000010: "RF_TagExp",
    0x00000020: "RF_SourceModified",
    0x00000040: "RF_TagGarbage",
    0x00000080: "RF_Final",
    0x00000100: "RF_PerObjectLocalized",
    0x00000200: "RF_NeedLoad",
    0x00000400: "RF_HighlightedName/RF_EliminateObject/RF_RemappedName/RF_Protected",
    0x00000800: "RF_InSingularFunc/RF_Suppress/RF_StateChanged",
    0x00001000: "RF_InEndState",
    0x00002000: "RF_Transient",
    0x00004000: "RF_Preloading",
    0x00008000: "RF_LoadForClient",
    0x00010000: "RF_LoadForServer",
    0x00020000: "RF_LoadForEdit",
    0x00040000: "RF_Standalone",
    0x00080000: "RF_NotForClient",
    0x00100000: "RF_NotForServer",
    0x00200000: "RF_NotForEdit",
    0x00400000: "RF_Destroyed",
    0x00800000: "RF_NeedPostLoad",
    0x01000000: "RF_HasStack",
    0x02000000: "RF_Native",
    0x04000000: "RF_Marked",
    0x08000000: "RF_ErrorShutdown",
    0x10000000: "RF_DebugPostLoad",
    0x20000000: "RF_DebugSerialize",
    0x40000000: "RF_DebugDestroy",
    0x80000000: "RF_DebugDestroy",
}

def extract_hex_values(text):
    """Extract all hex values starting with 0x from text"""
    return re.findall(r'0x([0-9a-fA-F]+)', text)

def analyze_flags(flag_value):
    """Analyze a flag value and return which flags are set"""
    if isinstance(flag_value, str):
        flag_value = int(flag_value, 16)
    
    set_flags = []
    for flag_bit, flag_name in sorted(flags_dict.items()):
        if flag_value & flag_bit:
            set_flags.append(flag_name)
    
    return set_flags

def analyze_multiple(flag_list):
    """Analyze multiple flags and find common patterns"""
    print("=" * 60)
    print("ANALYZING FLAGS")
    print("=" * 60)
    
    flag_counts = {}
    total = len(flag_list)
    
    # Count occurrences of each flag
    for flag_value in flag_list:
        if isinstance(flag_value, str):
            flag_value = int(flag_value, 16)
        
        for flag_bit, flag_name in flags_dict.items():
            if flag_value & flag_bit:
                flag_counts[flag_name] = flag_counts.get(flag_name, 0) + 1
    
    # Sort by frequency
    sorted_flags = sorted(flag_counts.items(), key=lambda x: x[1], reverse=True)
    
    print(f"\nFlag frequency across {total} files:\n")
    for flag_name, count in sorted_flags:
        percentage = (count / total) * 100
        print(f"{flag_name:50} {count:3}/{total} ({percentage:5.1f}%)")

if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("Usage: python flag_checker.py <filename>")
        sys.exit(1)
    
    filename = sys.argv[1]
    
    try:
        with open(filename, 'r') as f:
            content = f.read()
        
        # Extract all hex values
        hex_values = extract_hex_values(content)
        
        if not hex_values:
            print("No hex values found in file!")
            sys.exit(1)
        
        print(f"Found {len(hex_values)} hex values\n")
        
        # Analyze them
        analyze_multiple(hex_values)
        
    except FileNotFoundError:
        print(f"Error: File '{filename}' not found!")
        sys.exit(1)
    except Exception as e:
        print(f"Error: {e}")
        sys.exit(1)
