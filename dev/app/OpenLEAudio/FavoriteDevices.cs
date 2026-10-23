using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;
namespace OpenLEAudio;
internal static class FavoriteDevices
{
    private static readonly string FilePath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "OpenLEAudio", "favorites.json");
    private static HashSet<string> Read()
    {
        try { return new(JsonSerializer.Deserialize<string[]>(File.ReadAllText(FilePath)) ?? Array.Empty<string>(), StringComparer.OrdinalIgnoreCase); }
        catch { return new(StringComparer.OrdinalIgnoreCase); }
    }
    internal static string Key(string address) => new string(address.Where(Uri.IsHexDigit).ToArray()).ToUpperInvariant();
    public static bool Contains(string address) => Read().Any(saved => Key(saved)==Key(address));
    public static void Toggle(string address)
    {
        var items = Read(); var matches=items.Where(saved=>Key(saved)==Key(address)).ToArray();
        if(matches.Length==0)items.Add(address);else foreach(var match in matches)items.Remove(match);
        Directory.CreateDirectory(Path.GetDirectoryName(FilePath)!);
        var temporary = FilePath + ".tmp";
        File.WriteAllText(temporary, JsonSerializer.Serialize(items));
        File.Move(temporary, FilePath, true);
    }
}
