{ lib, rustPlatform, stdenv, python3, jujutsu, gitMinimal, openssh, rcodesign, runtimeShell }:

assert lib.assertMsg (lib.versionAtLeast jujutsu.version "0.47.1")
  "jj-repo-sync requires JJ 0.47.1 or newer for native reconciliation";

rustPlatform.buildRustPackage {
  pname = "jj-repo-sync";
  version = "2.0.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./build.rs ./src ./tests ./macos ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = lib.optionals stdenv.hostPlatform.isDarwin [ rcodesign ];
  nativeCheckInputs = [ python3 jujutsu gitMinimal openssh ];
  # Embed tool paths in the native executable; no script wrapper or interpreter.
  JJ_REPO_SYNC_JJ = "${jujutsu}/bin/jj";
  JJ_REPO_SYNC_GIT = "${gitMinimal}/bin/git";
  JJ_REPO_SYNC_SSH = "${openssh}/bin/ssh";
  JJ_REPO_SYNC_KEYGEN = "${openssh}/bin/ssh-keygen";
  JJ_REPO_SYNC_SHELL = runtimeShell;
  doCheck = true;
  postFixup = lib.optionalString stdenv.hostPlatform.isDarwin ''
    rcodesign sign --binary-identifier se.kjekac.jj-repo-sync \
      --timestamp-url none --signing-time 1970-01-01T00:00:00Z "$out/bin/jj-repo-sync"
  '';
  passthru = {
    supportsRepositoryAllowlist = true;
    nativeReconciliation = true;
    nativeExecutable = true;
    inherit jujutsu;
  };
  meta = {
    description = "Publish Jujutsu workspace tips and fetch paired repositories";
    mainProgram = "jj-repo-sync";
    license = lib.licenses.agpl3Plus;
    platforms = lib.platforms.unix;
  };
}
