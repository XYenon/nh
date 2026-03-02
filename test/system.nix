let
  system-manager = builtins.getFlake "github:numtide/system-manager";
in
{
  systemConfigs.default = system-manager.lib.makeSystemConfig {
    modules = [
      (
        { pkgs, ... }:
        {
          config = {
            nixpkgs.hostPlatform = "x86_64-linux";
            system-manager.allowAnyDistro = true;
            environment.systemPackages = [ pkgs.hello ];
          };
        }
      )
    ];
  };
}
