class PdfMcp < Formula
  desc "MCP server for AI agent interaction with PDF files"
  homepage "https://github.com/maxhodak/pdf-mcp"
  url "https://github.com/maxhodak/pdf-mcp/archive/refs/tags/v0.1.1.tar.gz"
  sha256 "3b816680c7dc6460dd00d31b1f7b146a97587d3c410690918a995f5438e1d245"
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
