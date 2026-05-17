class Ccal < Formula
  desc "Terminal calendar and notes app with Automerge sync"
  homepage "https://github.com/hillman/ccal"
  url "https://github.com/hillman/ccal/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_TARBALL_SHA256"
  license "GPL-3.0-or-later"
  head "https://github.com/hillman/ccal.git", branch: "master"

  depends_on "rust" => :build

  def install
    # Install only the user-facing binaries; import-bear is a one-off
    # migration tool and is intentionally left out of PATH.
    system "cargo", "install", "--bin", "ccal", "--bin", "ccal-server",
                               "--root", prefix, "--path", "."
  end

  test do
    # Neither binary parses CLI args (no --version/--help), and ccal-server
    # would bind a port, so assert the install layout instead of running them.
    assert_path_exists bin/"ccal"
    assert_path_exists bin/"ccal-server"
  end
end
