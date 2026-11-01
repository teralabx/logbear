class Logbear < Formula
  desc "Wrap a command, capture everything it emits, and push errors at your agent"
  homepage "https://logbear.sh"
  url "https://github.com/teralabx/logbear/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0" * 64 # replace at release time: see packaging/homebrew/README.md
  license "MIT"
  head "https://github.com/teralabx/logbear.git", branch: "master"

  depends_on "rust" => :build

  def install
    # Four binaries out of one workspace. `std_cargo_args` supplies --locked and
    # --root, so each `cargo install` lands in the same prefix.
    %w[crates/shim crates/daemon crates/mcp crates/hook].each do |member|
      system "cargo", "install", *std_cargo_args(path: member)
    end
  end

  service do
    run [opt_bin/"logbeard", "serve"]
    keep_alive true
    log_path var/"log/logbear/logbeard.log"
    error_log_path var/"log/logbear/logbeard.log"
  end

  def caveats
    <<~EOS
      Start the daemon so runs are captured across reboots:
        brew services start logbear

      Register the MCP server and the agent hook:
        logbear doctor

      The desktop app is a separate download from https://logbear.sh — this formula
      installs the CLI, the daemon, the MCP server and the hook only.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/logbear --version")

    # A wrapped command must reproduce its child's output byte for byte and
    # propagate the exit code, with no daemon running.
    ENV["LOGBEAR_HOME"] = testpath/"home"
    assert_equal "hello", shell_output("#{bin}/logbear /bin/sh -c 'printf hello'")
    shell_output("#{bin}/logbear /bin/sh -c 'exit 7'", 7)

    # The MCP server must speak JSON-RPC on stdio.
    handshake = <<~JSON
      {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"brew-test","version":"1"}}}
    JSON
    assert_match "protocolVersion",
                 pipe_output("#{bin}/logbear-mcp", handshake, 0)
  end
end
