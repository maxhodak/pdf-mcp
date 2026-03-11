class PdfMcp < Formula
  desc "MCP server for AI agent interaction with PDF files"
  homepage "https://github.com/maxhodak/pdf-mcp"
  url "https://github.com/maxhodak/pdf-mcp/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "54b776754d40314bb803622ed9eef6e8b9ebd5cf558c56567721781f45a3156e"
  license "AGPL-3.0-only"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    input = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
    output = pipe_output(bin/"pdf-mcp", input)
    assert_match "pdf-mcp", output
  end
end
