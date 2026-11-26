# GPUI 전환 — G1–G4 구현 배치 (부분 완료)

[보관된 실행 계획](../../../plan/gpui-terminal/TASKS.md) · [플래닝 보고](PLANNING.md)

- G1: macOS arm64에서 GPUI 0.2.2 / Ghostty VT 1.0.1 빌드 및 창 생성 확인. Tauri 소스 스냅샷 `/Users/dotdot/ddg-tauri-baseline.YUjoXV/source.tar.gz`를 만들고 lockfile 포함을 확인했다. 복구 실행은 미검증.
- G2/G3: 독립 Pi 세션에서 PTY와 Ghostty VT 모듈을 작성하고 GPUI에 통합했다. Rust 테스트 3개 통과: FIFO/리사이즈/종료, 큐 포화 중 종료, VT 분할 입력·키 인코딩·OSC 테스트.
- G4: 기본 셸, 독립 탭과 선택·닫기·재시작, 상태 사이드바, 너비 드래그를 연결했다. 실제 네이티브 창과 자식 `-zsh` 생성을 확인했다. 셀별 스타일·선택, 네이티브 IME, 키보드/VoiceOver 접근성과 종료 회수 동등성은 미완료.
- G5/G6: Pi 입력과 한글 IME 실기, 탭/창 종료, 복구 리허설을 포함한 전환 게이트 미충족. Tauri 기본 경로 및 소스를 보존했고 삭제·머지는 수행하지 않았다.
