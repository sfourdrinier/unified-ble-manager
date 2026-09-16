# scripts/ci/podspec-stub-eval.rb
#
# F01: evaluates the podspec selection logic without CocoaPods. Stubs
# Pod::Spec (attribute capture only), loads the one .podspec in --dir, and
# prints the captured attributes as JSON for
# check-podspec-rust-selection.js. Both the 5.x lane (real package.json)
# and a stubbed 4.x package.json run through this, proving the gate selects
# per lane instead of by marker presence.

require 'json'

dir = nil
ARGV.each_with_index do |arg, i|
  dir = ARGV[i + 1] if arg == '--dir'
end
abort 'usage: podspec-stub-eval.rb --dir <dir>' if dir.nil?

module Pod
  class SpecStub
    attr_reader :attrs

    def initialize
      @attrs = {}
    end

    def method_missing(name, *args)
      key = name.to_s
      if key.end_with?('=')
        @attrs[key[0..-2]] = args.first
      elsif key == 'dependency'
        (@attrs['dependencies'] ||= []) << args
      else
        @attrs[key]
      end
    end

    def respond_to_missing?(*)
      true
    end
  end

  module Spec
    def self.new
      stub = SpecStub.new
      yield stub
      $captured_spec_attrs = stub.attrs
    end
  end
end

podspecs = Dir.glob(File.join(dir, '*.podspec'))
abort "expected one .podspec in #{dir}, found #{podspecs.length}" unless podspecs.length == 1
load podspecs.first
puts JSON.generate($captured_spec_attrs)
