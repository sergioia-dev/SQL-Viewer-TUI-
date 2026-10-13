{
  description = "Terminal schema viewer for SQL databases";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Rust toolchain via fenix — pin the version so builds are reproducible
        rustToolchain = fenix.packages.${system}.stable.toolchain;

        # crane gives us incremental, cache-friendly Rust builds
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # sqlx's compile-time verification reads .sqlx/ and .sql files.
        # Without this filter, crane's default source filter would drop them.
        src = craneLib.cleanCargoSource (craneLib.path ./.);

        # Native libraries that every backend needs at *build* time.
        # sqlx links against these; the SQLite driver bundles libsqlite3
        # but still needs a C compiler, and Postgres/MySQL want their
        # client headers available for the -sys crates.
        nativeBuildInputs = with pkgs; [
          pkg-config
          rustToolchain
          sqlx-cli
        ];

        buildInputs = with pkgs; [
          openssl # TLS for Postgres/MySQL, and for tiberius
          sqlite # libsqlite3 — the sqlx SQLite driver links to it
          postgresql # libpq + headers for the postgres -sys crate
          mariadb # libmariadb + headers, used by sqlx-mysql
          # Uncomment when you add SQL Server support via tiberius:
          # krb5           # only needed if you enable integrated-auth-gssapi
        ];
      in
      {
        packages.default = craneLib.buildPackage {
          inherit src nativeBuildInputs buildInputs;
          pname = "sqlv";

          # The compiled binary ends up in result/bin/sqlv
          meta = with pkgs.lib; {
            description = "Render a SQL database schema as an interactive terminal diagram";
            license = licenses.mit;
            mainProgram = "sqlv";
          };
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;

          packages =
            nativeBuildInputs
            ++ (with pkgs; [
              # Editor and lint tooling
              rust-analyzer
              clippy
              rustfmt

              # Handy for testing against real databases without a server
              sqlite # the sqlite3 CLI
              postgresql # psql, initdb, pg_ctl
              mariadb # mysql client
            ]);

          # Lets rust-analyzer resolve the standard library sources.
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

          # sqlx-cli reads these when you run `sqlx prepare`.
          # Point them at a local database of your choice, or leave
          # them unset and use `cargo sqlx prepare --workspace` manually.
          # DATABASE_URL = "postgres://localhost/schema_viewer_dev";

          # Tell the sqlx -sys crates where the libraries live.
          # This is the part that most often trips people up.
          OPENSSL_NO_VENDOR = "1";
          PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" [
            pkgs.openssl.dev
            pkgs.postgresql.lib
            pkgs.mariadb
          ];

          shellHook = ''
            echo "sqlv dev shell"
            echo "  rustc $(rustc --version | cut -d' ' -f2)"
            echo "  sqlx-cli $(sqlx --version | cut -d' ' -f2)"
            echo ""
            echo "Set DATABASE_URL before running 'sqlx prepare'."
          '';
        };
      }
    );
}
