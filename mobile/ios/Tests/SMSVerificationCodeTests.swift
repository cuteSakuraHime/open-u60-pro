import Foundation

// Compile alongside Features/SMS/SMSModels.swift as a standalone executable.
@main
struct SMSVerificationCodeTests {
    static func main() {
        let examples: [(String, String?)] = [
            ("【测试银行】验证码：012345，5分钟内有效。", "012345"),
            ("您的驗證碼為 987654。", "987654"),
            ("您的验证码是012345请勿泄露", "012345"),
            ("Your verification code is 123456. Expires in 5 minutes.", "123456"),
            ("654321 is your verification code.", "654321"),
            ("OTP: 1234", "1234"),
            ("Your one-time password: 87654321", "87654321"),
            ("订单号 123456，预计明日送达。", nil),
            ("验证码已发送至 12345678901", nil),
            ("验证码：123456789", nil),
            ("验证码：AB123456CD", nil),
            ("验证码为 123456，另一个验证码为 654321", nil),
            ("验证码已失效。订单号 123456", nil),
            ("验证码：123456，有效期5分钟，请勿泄露验证码。", "123456"),
            ("", nil)
        ]
        for (text, expected) in examples {
            let actual = SMSParser.verificationCode(in: text)
            precondition(actual == expected, "Unexpected extraction for \(text): \(String(describing: actual))")
        }
        for tag in [SMSTag.sent, .draft, .failed] {
            let message = SMSMessage(id: 1, number: "TEST", content: "验证码：123456",
                                     date: Date(), tag: tag, groupId: "", memStore: "nv")
            precondition(message.verificationCode == nil, "Outgoing messages must not surface a code")
        }
        let encoded = SMSParser.encodeUCS2Hex("验证码：012345")
        let parsed = SMSParser.parseMessages(["messages": [[
            "id": 1, "number": "TEST", "content": encoded,
            "date": "26,11,10,02,00,00,+32", "tag": "1"
        ]]])
        precondition(parsed.first?.verificationCode == "012345", "Decode UCS-2 before extraction")
        print("SMSVerificationCodeTests passed")
    }
}
