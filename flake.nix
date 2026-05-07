{
  description = "unrealin development shell with .NET 8 for Unreal-Library validation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forAllSystems = f: nixpkgs.lib.genAttrs [ "x86_64-darwin" "aarch64-darwin" "x86_64-linux" "aarch64-linux" ] (system: f system);
    in {
      devShells = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.dotnet-sdk_9 ];
            shellHook = ''
              export DOTNET_NOLOGO=1
              export DOTNET_CLI_TELEMETRY_OPTOUT=1
              export DOTNET_ROOT="${pkgs.dotnet-sdk_9}/share/dotnet"
            '';
          };
        });
    };
}
